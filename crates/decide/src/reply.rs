//! End-of-turn reply question: after the user's turn on the primary microphone, should Heimdall say one of its
//! pre-generated voice-bank lines out loud right now, and which one? `none` is the default answer.
//!
//! The question runs as its own single-question d1 request (same transcript as the turn's decision), so
//! it never changes the SendNow request or the command chain. The native distribution is kept unchanged;
//! the policy below calibrates it and applies the threshold.
use crate::{Answer, Candidate, DecisionInput, LocalDecider, Question};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// The "say nothing" option.
pub const NONE: &str = "none";
pub const INSTRUCTIONS: &str = "Should Heimdall reply out loud right now, and with which line? Choose none unless the user is talking to Heimdall and one of these short lines is a natural immediate reply.";
/// `models/decide/reply-policy.json`, optional; absent means [`ReplyPolicy::default`].
pub const POLICY_FILE: &str = "reply-policy.json";

/// Options: `none` plus each line (`id`, spoken text). The spoken text is the option's description.
pub fn question<'a>(lines: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<Question> {
    let mut options = vec![Candidate { id: NONE.into(), description: "Say nothing".into() }];
    options.extend(lines.into_iter().map(|(id, text)| Candidate { id: id.into(), description: format!("Say \"{text}\"") }));
    let q = Question::ReplyLine(options);
    q.validate()?;
    Ok(q)
}

/// Calibration and threshold. `temperature` rescales the native distribution (p^(1/T), renormalized);
/// 1.0 leaves it unchanged until a fitted value exists. A line plays only when its calibrated probability
/// is the maximum and reaches `threshold`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplyPolicy {
    pub threshold: f32,
    pub temperature: f32,
}
impl Default for ReplyPolicy {
    fn default() -> Self {
        Self { threshold: 0.8, temperature: 1.0 }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplyChoice {
    /// The line to say; None means say nothing.
    pub line: Option<String>,
    /// The calibrated argmax option and its probability.
    pub top: String,
    pub probability: f32,
    pub calibrated: BTreeMap<String, f32>,
    /// `model_chose_none`, `below_threshold` or `above_threshold`.
    pub reason: String,
}
impl ReplyPolicy {
    pub fn load(models: &Path) -> Result<Self> {
        let path = models.join(POLICY_FILE);
        if !path.is_file() {
            return Ok(Self::default());
        }
        let p: Self = serde_json::from_slice(&std::fs::read(&path)?).context("read reply policy")?;
        ensure!(
            p.threshold.is_finite() && (0.0..=1.0).contains(&p.threshold) && p.temperature.is_finite() && p.temperature > 0.,
            "invalid reply policy"
        );
        Ok(p)
    }
    pub fn calibrate(&self, native: &BTreeMap<String, f32>) -> BTreeMap<String, f32> {
        let scaled: BTreeMap<_, _> =
            native.iter().map(|(k, p)| (k.clone(), p.max(0.).powf(1. / self.temperature))).collect();
        let sum: f32 = scaled.values().sum();
        scaled.into_iter().map(|(k, p)| (k, if sum > 0. { p / sum } else { 0. })).collect()
    }
    pub fn choose(&self, native: &BTreeMap<String, f32>) -> ReplyChoice {
        let calibrated = self.calibrate(native);
        // Ties go to none: saying nothing is the default.
        let (top, probability) = calibrated
            .iter()
            .fold((NONE.to_string(), calibrated.get(NONE).copied().unwrap_or(0.)), |best, (k, p)| {
                if *p > best.1 { (k.clone(), *p) } else { best }
            });
        let (line, reason) = if top == NONE {
            (None, "model_chose_none")
        } else if probability < self.threshold {
            (None, "below_threshold")
        } else {
            (Some(top.clone()), "above_threshold")
        };
        ReplyChoice { line, top, probability, calibrated, reason: reason.into() }
    }
}
/// The reply answer's native distribution.
pub fn distribution(answers: &[Answer]) -> Option<&BTreeMap<String, f32>> {
    answers.iter().find_map(|a| match a {
        Answer::ReplyLine { probabilities, .. } => Some(probabilities),
        _ => None,
    })
}

impl LocalDecider {
    /// The reply question alone: d1 only, with no TCL history, send head or training-archive side effects.
    /// Returns the typed answer and the raw native output for the caller's evidence record.
    pub fn reply_answer(&mut self, input: &DecisionInput) -> (Result<(Vec<Answer>, u64)>, Option<serde_json::Value>) {
        self.d1.last_output = None;
        let result = (|| {
            ensure!(
                matches!(input.questions.as_slice(), [Question::ReplyLine(_)]),
                "reply question runs alone"
            );
            self.d1.answer(input, 0.0)
        })();
        (result, self.d1.last_output.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn dist(pairs: &[(&str, f32)]) -> BTreeMap<String, f32> {
        pairs.iter().map(|(k, p)| (k.to_string(), *p)).collect()
    }
    #[test]
    fn says_nothing_by_default_below_threshold_or_on_ties() {
        let p = ReplyPolicy::default();
        let c = p.choose(&dist(&[("none", 0.7), ("sure", 0.2), ("okay", 0.1)]));
        assert_eq!((c.line, c.reason.as_str()), (None, "model_chose_none"));
        let c = p.choose(&dist(&[("none", 0.3), ("sure", 0.6), ("okay", 0.1)]));
        assert_eq!((c.line, c.top.as_str(), c.reason.as_str()), (None, "sure", "below_threshold"));
        let c = p.choose(&dist(&[("none", 0.05), ("sure", 0.9), ("okay", 0.05)]));
        assert_eq!((c.line.as_deref(), c.reason.as_str()), (Some("sure"), "above_threshold"));
        assert!((c.probability - 0.9).abs() < 1e-6);
        let c = p.choose(&dist(&[("none", 0.5), ("sure", 0.5)]));
        assert_eq!(c.line, None);
        // A sharper calibration can lift a line over the threshold; the native values stay as given.
        let sharp = ReplyPolicy { threshold: 0.8, temperature: 0.5 };
        let c = sharp.choose(&dist(&[("none", 0.2), ("sure", 0.7), ("okay", 0.1)]));
        assert_eq!(c.line.as_deref(), Some("sure"));
        assert!((c.calibrated.values().sum::<f32>() - 1.).abs() < 1e-5);
    }
    #[test]
    fn question_lists_none_first_and_parses_native_answers() -> Result<()> {
        let q = question([("sure", "Sure."), ("okay", "Okay.")])?;
        let Question::ReplyLine(options) = &q else { panic!() };
        assert_eq!(options.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["none", "sure", "okay"]);
        assert_eq!(q.key(), "reply_line");
        assert!(Question::ReplyLine(options[1..].to_vec()).validate().is_err(), "none is mandatory");
        let mut input: DecisionInput = serde_json::from_value(serde_json::json!({
            "source":"microphone","is_final":true,
            "current":{"id":"u1","role":{"role":"owner"},"transcript":"can you check the build","audio":{"samples":[0.0,0.0],"sample_rate":16000}},
            "context":[],"pending_transcript":"","agent_state":"","media":{"kind":"audio"},"questions":[]
        }))?;
        input.questions = vec![q];
        let rendered = crate::D1::questions(&input);
        assert_eq!(rendered["reply_line"]["instructions"], INSTRUCTIONS);
        assert_eq!(rendered["reply_line"]["criteria"]["sure"], "Say \"Sure.\"");
        let native = serde_json::json!({"usage":{"output_tokens":0,"input_tokens":120},
            "answers":{"reply_line":{"choice":"sure","probabilities":{"none":0.1,"sure":0.85,"okay":0.05}}}});
        let (answers, tokens) = crate::d1::parse_for_tests(&input, &native)?;
        assert_eq!(tokens, 120);
        assert_eq!(distribution(&answers).unwrap()["sure"], 0.85);
        let wrong = serde_json::json!({"usage":{"output_tokens":0,"input_tokens":1},
            "answers":{"reply_line":{"choice":"okay","probabilities":{"none":0.1,"sure":0.85,"okay":0.05}}}});
        assert!(crate::d1::parse_for_tests(&input, &wrong).is_err(), "choice must be the maximum");
        Ok(())
    }
}
