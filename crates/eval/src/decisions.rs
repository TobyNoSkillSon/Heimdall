use anyhow::Result;
use decide::{
    Answer, AudioClip, Decider, DecisionInput, DecisionMedia, DecisionSource, FixedCommand,
    LocalDecider, PendingThought, Question, Role, Turn, D1,
};
use listen::Word;
use screen::{PhraseMatcher, TimedWord};
use serde_json::{json, Value};
use std::{path::Path, time::Instant};

/// Offline-only: invokes no capture, screen permission, agent session or network API.
pub struct Decisions {
    runtime: Option<LocalDecider>,
    direct: Option<D1>,
    pub status: String,
    context: Vec<Turn>,
    pending: PendingThought,
    pub phrases: Vec<Value>,
    matcher: PhraseMatcher,
    last_decision: Option<Value>,
}
impl Decisions {
    pub fn load(models: &Path, archive: &Path) -> Self {
        match LocalDecider::load(models.join("decide"), archive) {
            Ok(runtime) => Self {
                runtime: Some(runtime),
                direct: None,
                status: "local_tcl_and_d1".into(),
                context: vec![],
                pending: PendingThought::default(),
                phrases: vec![],
                matcher: PhraseMatcher::default(),
                last_decision: None,
            },
            Err(e) => Self {
                runtime: None,
                direct: None,
                status: format!("decide_unavailable: {e:#}"),
                context: vec![],
                pending: PendingThought::default(),
                phrases: vec![],
                matcher: PhraseMatcher::default(),
                last_decision: None,
            },
        }
    }
    pub fn load_collection(models: &Path, archive: &Path) -> Result<Self> {
        // Legacy collection still pins the original checkpoint, but uses the
        // same text-only boundary as live replay. Historical audio feature
        // receipts require the separate native reference replay.
        let m = models.join("decide");
        let direct = D1::load(
            m.join("runtime/libheimdall_decide.dylib"),
            m.join("d1/d1-omni-600M-F16.gguf"),
        )?;
        let mut s = Self::load_unavailable(archive);
        s.direct = Some(direct);
        s.status = "d1_feature_collection_tcl_not_run".into();
        Ok(s)
    }
    fn load_unavailable(_archive: &Path) -> Self {
        Self {
            runtime: None,
            direct: None,
            status: "unavailable".into(),
            context: vec![],
            pending: PendingThought::default(),
            phrases: vec![],
            matcher: PhraseMatcher::default(),
            last_decision: None,
        }
    }
    pub fn available(&self) -> bool {
        self.runtime.is_some() || self.direct.is_some()
    }
    pub fn reset(&mut self, id: &str) {
        self.context.clear();
        self.pending.sent();
        self.last_decision = None;
        self.phrases.clear();
        self.matcher.reset_source(id);
    }
    pub fn observe(&mut self, id: &str, words: &[Word]) -> Result<()> {
        let timed: Vec<_> = words
            .iter()
            .map(|w| TimedWord {
                text: w.text.clone(),
                start_ns: w.start_ns,
                end_ns: w.end_ns,
            })
            .collect();
        for p in self.matcher.observe(id, &timed) {
            self.phrases.push(serde_json::to_value(p)?);
        }
        Ok(())
    }
    pub fn quiet_send(&mut self, _silence_ms: u64) -> Option<Value> { None }
    pub fn evaluate(&mut self, id: &str, index: usize, final_: &Value, audio: &[f32]) -> Value {
        if !self.available() {
            return json!({"status":self.status,"send_now":null,"screenshot":null});
        }
        if let Err(e) = self.pending.append(final_["text"].as_str().unwrap_or("")) {
            return json!({"status":"decision_error","error":e.to_string(),"send_now":null});
        }
        let start = Instant::now();
        let from = (final_["start_ns"].as_u64().unwrap_or(0) / 62500) as usize;
        let end = ((final_["end_ns"].as_u64().unwrap_or(0) / 62500) as usize).min(audio.len());
        // LocalDecider's public API caps audio at 30s. Retain the most recent 30s;
        // report this limitation explicitly rather than silently truncating input.
        let begin = from.max(end.saturating_sub(decide::MAX_AUDIO));
        let result = (|| -> Result<_> {
            let clip = AudioClip::new(
                audio
                    .get(begin..end)
                    .unwrap_or(&[])
                    .iter()
                    .map(|x| x.clamp(-1.0, 1.0))
                    .collect(),
            )?;
            let current = Turn {
                id: format!("{id}:{index}"),
                role: Role::Owner,
                transcript: final_["text"].as_str().unwrap_or("").into(),
                audio: clip,
            };
            let input = DecisionInput {
                source: DecisionSource::Microphone,
                is_final: true,
                current: current.clone(),
                context: self.context.clone(),
                pending_transcript: self.pending.text(),
                agent_state:
                    "DRY-RUN evaluation: agent replies and prior recording context are unavailable"
                        .into(),
                media: DecisionMedia::Audio,
                questions: vec![Question::SendNow, Question::FixedCommand],
            };
            let d = if let Some(runtime) = &mut self.runtime {
                runtime.decide(&input)?
            } else {
                let t = Instant::now();
                let (answers, input_tokens) = self.direct.as_mut().unwrap().answer(&input, 0.0)?;
                let raw = answers.iter().find_map(|a| {
                    if let Answer::SendNow { probability } = a {
                        Some(*probability)
                    } else {
                        None
                    }
                });
                decide::Decision {
                    utterance_id: input.current.id.clone(),
                    addressed_probability: 0.0,
                    addressedness_advisory: true,
                    addressed_error: None,
                    addressed_skip_reason: Some("training_collection".into()),
                    raw_send_probability: raw,
                    answers,
                    input_tokens,
                    runtime: "d1-feature-collection;TCL-not-run".into(),
                    latency: decide::Latency {
                        addressed_ms: 0.0,
                        decisions_ms: t.elapsed().as_secs_f64() * 1000.0,
                        archive_ms: 0.0,
                        total_ms: t.elapsed().as_secs_f64() * 1000.0,
                    },
                }
            };
            self.context.push(current);
            if self.context.len() > decide::MAX_CONTEXT {
                self.context.remove(0);
            }
            Ok(d)
        })();
        match result {
            Ok(d) => {
                let mut send = None;
                let mut send_ungated = None;
                let mut screenshot = None;
                for a in &d.answers {
                    match a {
                        Answer::SendNow { probability } => {
                            send_ungated = Some(*probability >= 0.5);
                            send = Some(*probability >= 0.5)
                        }
                        Answer::FixedCommand { command, .. } => {
                            screenshot = Some(matches!(
                                command,
                                FixedCommand::TakeScreenshot | FixedCommand::LookAtThis
                            ))
                        }
                        _ => {}
                    }
                }
                self.last_decision = Some(serde_json::to_value(&d).unwrap());
                let pending_transcript = self.pending.text();
                if send == Some(true) && self.direct.is_none() {
                    self.pending.sent();
                }
                json!({"status":self.status,"pending_transcript":pending_transcript,"send_now":send,"send_now_ungated":send_ungated,"screenshot":screenshot,"decision":d,"audio_tail_truncated":begin>from,
                "decision_audio_start_ns":begin as u64*62500,"decision_audio_samples":end.saturating_sub(begin),"elapsed_ms":start.elapsed().as_secs_f64()*1000.0})
            }
            Err(e) => {
                json!({"status":"decision_error","error":format!("{e:#}"),"send_now":null,"screenshot":null,"elapsed_ms":start.elapsed().as_secs_f64()*1000.0})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quiet_check_never_sends_retained_thought() {
        let mut d = Decisions::load_unavailable(Path::new("unused"));
        d.pending.append("Still thinking because").unwrap();
        assert!(d.quiet_send(2499).is_none());
        assert!(d.quiet_send(2500).is_none());
        assert_eq!(d.pending.text(), "Still thinking because");
        assert!(d.quiet_send(9000).is_none());
    }
}
