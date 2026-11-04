//! Barge-in: the user talks on the primary microphone while Heimdall is speaking. After about a second of his
//! speech the app transcribes it on the side and asks this one question; Heimdall stops unless the decider is
//! confident it should keep talking ("vast majority of the time, yes, you should stop. But then
//! again, we rely on the decider again"). Text only, one question, no TCL, send head or training-archive
//! side effects; the app journals each decision.
use crate::{Answer, AudioClip, DecisionInput, DecisionMedia, DecisionSource, LocalDecider, Question, Role, Turn};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const INSTRUCTIONS: &str = "Heimdall is speaking. User just said this. Should Heimdall stop talking now?";
pub const STOP: &str = "stop";
pub const CONTINUE: &str = "continue";
/// Heimdall keeps talking only when the native probability of `continue` reaches this; anything less,
/// including a tie or an argmax of `continue` below it, stops. Stopping is the default.
pub const CONTINUE_THRESHOLD: f32 = 0.8;
pub const POLICY: &str = "barge-in-v1;text-only;side-transcript;stop-unless-continue>=0.8";

/// The two options and what each means, as the decider reads them.
pub fn criteria() -> BTreeMap<&'static str, &'static str> {
    BTreeMap::from([
        (STOP, "User is talking to Heimdall or over it: interrupting, answering, correcting, asking or redirecting."),
        (CONTINUE, "Background speech, someone else talking, or an aside not meant for Heimdall."),
    ])
}

/// The question's input: what the microphone heard and what Heimdall is saying. Nothing else, to stay fast.
pub fn input(id: &str, heard: &str, heimdall_saying: &str) -> Result<DecisionInput> {
    let saying: String = {
        // The latest words matter most; keep the tail within the bounded state.
        let chars: Vec<char> = heimdall_saying.chars().collect();
        chars[chars.len().saturating_sub(600)..].iter().collect()
    };
    let input = DecisionInput {
        source: DecisionSource::Microphone,
        is_final: false,
        current: Turn {
            id: id.into(),
            role: Role::Owner,
            transcript: heard.into(),
            // Audio is transcribed upstream; no clip is passed to d1.
            audio: AudioClip::default(),
        },
        context: vec![],
        pending_transcript: String::new(),
        agent_state: format!("Heimdall is saying: {saying}"),
        media: DecisionMedia::Text,
        questions: vec![Question::BargeIn],
    };
    input.validate()?;
    Ok(input)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub stop: bool,
    /// The decider's own pick (its argmax), before the policy.
    pub native_choice: String,
    pub continue_probability: f32,
    pub probabilities: BTreeMap<String, f32>,
}
/// The policy over the decider's answer.
pub fn verdict(answers: &[Answer]) -> Result<Verdict> {
    let (choice, probabilities) = answers
        .iter()
        .find_map(|a| match a {
            Answer::BargeIn { choice, probabilities } => Some((choice, probabilities)),
            _ => None,
        })
        .context("no barge-in answer")?;
    let p = *probabilities.get(CONTINUE).context("no continue probability")?;
    Ok(Verdict { stop: p < CONTINUE_THRESHOLD, native_choice: choice.clone(), continue_probability: p, probabilities: probabilities.clone() })
}

impl LocalDecider {
    /// The barge-in question alone, from text. Returns the typed answer and the raw native output.
    pub fn barge_in_answer(&mut self, input: &DecisionInput) -> (Result<(Vec<Answer>, u64)>, Option<serde_json::Value>) {
        self.d1.last_output = None;
        let result = (|| {
            ensure!(matches!(input.media, DecisionMedia::Text), "barge-in reads text only");
            ensure!(matches!(input.questions.as_slice(), [Question::BargeIn]), "barge-in question runs alone");
            self.d1.answer(input, 0.0)
        })();
        (result, self.d1.last_output.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn answer(choice: &str, p_continue: f32) -> Vec<Answer> {
        vec![Answer::BargeIn {
            choice: choice.into(),
            probabilities: [(STOP.to_string(), 1. - p_continue), (CONTINUE.to_string(), p_continue)].into(),
        }]
    }
    #[test]
    fn stops_unless_confident_it_should_continue() -> Result<()> {
        assert!(verdict(&answer(STOP, 0.1))?.stop);
        assert!(verdict(&answer(STOP, 0.5))?.stop, "a tie stops");
        let v = verdict(&answer(CONTINUE, 0.7))?;
        assert!(v.stop && v.native_choice == CONTINUE, "continue below the threshold still stops");
        assert!(!verdict(&answer(CONTINUE, 0.8))?.stop);
        assert!(!verdict(&answer(CONTINUE, 0.97))?.stop);
        assert!(verdict(&[]).is_err());
        Ok(())
    }
    #[test]
    fn question_renders_and_parses_both_options() -> Result<()> {
        let long = "word ".repeat(400);
        let input = input("barge-1", "wait, no, the other file", &long)?;
        assert!(input.agent_state.len() < 700, "Heimdall's text is bounded to its tail");
        let rendered = crate::D1::questions(&input);
        assert_eq!(rendered["barge_in"]["instructions"], INSTRUCTIONS);
        assert_eq!(rendered["barge_in"]["criteria"].as_object().unwrap().len(), 2);
        assert!(serde_json::to_string(&crate::D1::request(&input, 0.0)?)?.contains("wait, no, the other file"));
        let native = serde_json::json!({"usage":{"output_tokens":0,"input_tokens":80},
            "answers":{"barge_in":{"choice":"stop","probabilities":{"stop":0.9,"continue":0.1}}}});
        let (answers, tokens) = crate::d1::parse_for_tests(&input, &native)?;
        assert_eq!(tokens, 80);
        assert!(verdict(&answers)?.stop);
        let wrong = serde_json::json!({"usage":{"output_tokens":0,"input_tokens":1},
            "answers":{"barge_in":{"choice":"continue","probabilities":{"stop":0.9,"continue":0.1}}}});
        assert!(crate::d1::parse_for_tests(&input, &wrong).is_err(), "choice must be the maximum");
        Ok(())
    }
}
