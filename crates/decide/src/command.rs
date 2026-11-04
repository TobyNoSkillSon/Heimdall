//! Fixed, fail-closed command decisions, independent of capture and agent harnesses.
use crate::{Answer, DecisionInput, DecisionMedia, Question};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// A phrase proposal is evidence, never authority to act. Times use capture's host clock.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandCandidate {
    pub source: String,
    pub trigger_ns: u64,
    pub phrase_guess: String,
    pub score: f32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandIntent {
    Yes,
    FalseAlarm,
    Unsure,
}
impl CommandIntent {
    pub const ALL: [Self; 3] = [Self::Yes, Self::FalseAlarm, Self::Unsure];
    pub fn key(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::FalseAlarm => "false_alarm",
            Self::Unsure => "unsure",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandSelection {
    TakeScreenshot,
    LookAtThis,
    OtherCommand,
    Unsure,
}
impl CommandSelection {
    /// Decider V5's commands are the two screen commands (cancel-task removed 29 Oct;).
    pub const ALL: [Self; 4] = [
        Self::TakeScreenshot,
        Self::LookAtThis,
        Self::OtherCommand,
        Self::Unsure,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::TakeScreenshot => "take_screenshot",
            Self::LookAtThis => "look_at_this",
            Self::OtherCommand => "other_command",
            Self::Unsure => "unsure",
        }
    }
    pub fn description(self) -> &'static str {
        match self {
            Self::TakeScreenshot => "Take a screenshot: capture the screen.",
            Self::LookAtThis => {
                "Look at this, take a look at this, check this out: inspect the current screen."
            }
            Self::OtherCommand => "A different action is requested.",
            Self::Unsure => "Cannot identify the action.",
        }
    }
    pub fn listed(self) -> bool {
        !matches!(self, Self::OtherCommand | Self::Unsure)
    }
    /// The early and final stages' first question: concise, about this one command, at the trigger.
    pub fn check_question(self) -> &'static str {
        match self {
            Self::TakeScreenshot => "Did User, right now, ask Heimdall to take a screenshot?",
            Self::LookAtThis => "Did User, right now, ask Heimdall to look at his screen?",
            Self::OtherCommand | Self::Unsure => "Did User, right now, ask Heimdall to do something?",
        }
    }
    /// The command a detected phrase proposes (English or Polish, any listed alias).
    pub fn for_phrase(phrase: &str) -> Self {
        let p = command_transcript_view(phrase).0.to_lowercase();
        let has = |w: &str| p.contains(w);
        if has("screenshot") || has("capture") || has("zrzut") || has("zdjęcie") {
            Self::TakeScreenshot
        } else {
            Self::LookAtThis
        }
    }
}
/// Where a command decision is made: at first detection from the audio (no final transcript yet, so
/// no transcript veto), or when the turn's final arrives (transcript veto applies).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStage {
    Early,
    #[default]
    Final,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandStep {
    pub question: Question,
    pub instructions: Value,
    pub state: String,
    pub answers: Vec<Answer>,
    pub model_output: Option<Value>,
    pub input_tokens: u64,
    #[serde(default)]
    pub d1_ms: f64,
    pub error: Option<String>,
    /// Policy choice; native answers and output remain untouched above.
    #[serde(default)]
    pub policy_choice: Option<String>,
    #[serde(default)]
    pub policy_probabilities: Option<BTreeMap<String, f32>>,
    #[serde(default)]
    pub policy_veto: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandDecision {
    pub schema: u32,
    #[serde(default)]
    pub policy: String,
    #[serde(default)]
    pub stage: CommandStage,
    #[serde(default)]
    pub aliases: Vec<CommandAlias>,
    pub candidate: CommandCandidate,
    pub utterance_id: String,
    pub steps: Vec<CommandStep>,
    pub intent: Option<CommandIntent>,
    pub selection: Option<CommandSelection>,
    /// Approved action, separate from successful execution (recorded by the app).
    pub approved: Option<CommandSelection>,
    pub error: Option<String>,
}
impl CommandDecision {
    pub fn needs_training(&self) -> bool {
        self.intent == Some(CommandIntent::Unsure)
            || matches!(
                self.selection,
                Some(CommandSelection::OtherCommand | CommandSelection::Unsure)
            )
            || self.error.is_some()
    }
}
/// One text question per command, decided at detection from a side transcript of the last <=15 s; native
/// escapes are never promoted. No audio reaches the decider and no transcript rule overrides it.
pub const COMMAND_POLICY: &str =
    "command-check-v9;text-only;side-transcript-15s;single-question;native-escapes;no-transcript-veto;v5-two-screen-commands";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandAlias {
    pub heard: String,
    pub canonical: String,
    pub start_byte: usize,
    pub end_byte: usize,
}
/// A command-only English gloss. Keep all surrounding text, including quotation
/// and negation; the app archives the original final and these explicit edits.
pub fn command_transcript_view(text: &str) -> (String, Vec<CommandAlias>) {
    let mut words = Vec::new();
    let mut start = None;
    for (at, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if ch.is_alphanumeric() {
            if start.is_none() {
                start = Some(at);
            }
        } else if let Some(a) = start.take() {
            words.push((a, at, text[a..at].to_lowercase()));
        }
    }
    let aliases: &[(&[&str], &str)] = &[
        (&["zrób", "zrzut", "ekranu"], "take a screenshot"),
        (&["zobacz", "to"], "look at this"),
        (&["spójrz"], "look at this"),
    ];
    let mut found = Vec::new();
    let mut i = 0;
    while i < words.len() {
        let mut matched = false;
        for &(phrase, canonical) in aliases {
            if i + phrase.len() > words.len()
                || !phrase.iter().enumerate().all(|(j, w)| words[i + j].2 == *w)
            {
                continue;
            }
            let a = words[i].0;
            let b = words[i + phrase.len() - 1].1;
            // Do not join aliases across intervening sentence punctuation.
            if (i..i + phrase.len() - 1).any(|j| {
                !text[words[j].1..words[j + 1].0]
                    .chars()
                    .all(char::is_whitespace)
            }) {
                continue;
            }
            found.push(CommandAlias {
                heard: text[a..b].into(),
                canonical: canonical.into(),
                start_byte: a,
                end_byte: b,
            });
            i += phrase.len();
            matched = true;
            break;
        }
        if !matched {
            i += 1;
        }
    }
    let mut view = String::new();
    let mut at = 0;
    for alias in &found {
        view.push_str(&text[at..alias.start_byte]);
        view.push_str(&alias.canonical);
        at = alias.end_byte;
    }
    view.push_str(&text[at..]);
    (view, found)
}
/// Feed the proposal as fallible evidence, while retaining all action/escape
/// options. It helps the fixed selector locate the command inside a long final.
pub fn command_input(candidate: &CommandCandidate, input: &DecisionInput) -> DecisionInput {
    let mut next = input.clone();
    next.current.transcript = command_transcript_view(&input.current.transcript).0;
    let mut phrase = command_transcript_view(&candidate.phrase_guess).0;
    for alias in command_transcript_view(&input.current.transcript).1 {
        if alias.heard.eq_ignore_ascii_case(&candidate.phrase_guess) {
            phrase = alias.canonical;
            break;
        }
    }
    let original_view = next.current.transcript.clone();
    let mut focused = false;
    if matches!(input.questions.as_slice(), [Question::CommandSelection]) {
        let tokens: Vec<_> = phrase
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .collect();
        let mut words = Vec::new();
        let mut start = None;
        for (at, ch) in original_view
            .char_indices()
            .chain(std::iter::once((original_view.len(), ' ')))
        {
            if ch.is_alphanumeric() {
                if start.is_none() {
                    start = Some(at);
                }
            } else if let Some(a) = start.take() {
                words.push((a, at, original_view[a..at].to_lowercase()));
            }
        }
        if !tokens.is_empty() {
            for i in 0..words.len() {
                if i + tokens.len() <= words.len()
                    && tokens.iter().enumerate().all(|(j, t)| words[i + j].2 == *t)
                {
                    next.current.transcript =
                        original_view[words[i].0..words[i + tokens.len() - 1].1].into();
                    focused = true;
                    break;
                }
            }
        }
    }
    next.agent_state = serde_json::json!({
        "agent_state": input.agent_state,
        "command_proposal": {"phrase_guess": phrase, "score": candidate.score},
        "proposal_is_fallible": true,
        "original_utterance": original_view,
        "selection_uses_verified_phrase": focused
    })
    .to_string();
    next
}
fn validate_probabilities(p: &BTreeMap<String, f32>, keys: &[&str], chosen: &str) -> Result<()> {
    ensure!(
        p.len() == keys.len() && keys.iter().all(|k| p.contains_key(*k)),
        "incomplete command distribution"
    );
    ensure!(
        p.values().all(|v| v.is_finite() && (0.0..=1.0).contains(v)),
        "invalid command distribution"
    );
    ensure!(
        (p.values().sum::<f32>() - 1.0).abs() <= 0.001,
        "command distribution does not sum to one"
    );
    ensure!(
        p.values().all(|v| *v <= p[chosen] + 1e-6),
        "native command choice is not maximal"
    );
    Ok(())
}
/// One question: did the user, right now, ask for the command his phrase proposes? Native yes approves that
/// command; false_alarm and unsure end the chain. The closure returns the native output even on parse
/// failure, for archival.
pub fn command_chain(
    candidate: CommandCandidate,
    input: &DecisionInput,
    answer: impl FnMut(&DecisionInput) -> (Result<(Vec<Answer>, u64)>, Option<Value>),
) -> CommandDecision {
    command_chain_at(candidate, input, CommandStage::Final, answer)
}
/// `stage` records where the decision was made (at detection, or at the turn's final when no side
/// transcript was ready); the decision itself is the same.
pub fn command_chain_at(
    candidate: CommandCandidate,
    input: &DecisionInput,
    stage: CommandStage,
    mut answer: impl FnMut(&DecisionInput) -> (Result<(Vec<Answer>, u64)>, Option<Value>),
) -> CommandDecision {
    let command = CommandSelection::for_phrase(&candidate.phrase_guess);
    let mut record = CommandDecision {
        schema: 1,
        policy: COMMAND_POLICY.into(),
        stage,
        aliases: vec![],
        candidate,
        utterance_id: input.current.id.clone(),
        steps: vec![],
        intent: None,
        selection: None,
        approved: None,
        error: None,
    };
    let result = (|| -> Result<()> {
        ensure!(
            record.candidate.score.is_finite() && (0.0..=1.0).contains(&record.candidate.score),
            "invalid candidate score"
        );
        ensure!(!record.candidate.source.is_empty(), "missing candidate source");
        ensure!(
            input.source == crate::DecisionSource::Microphone && matches!(input.current.role, crate::Role::Owner),
            "commands require User's microphone"
        );
        ensure!(matches!(input.media, DecisionMedia::Text), "command decisions read text only");
        let (_, aliases) = command_transcript_view(&input.current.transcript);
        record.aliases = aliases;
        let question = Question::CommandCheck(command);
        let mut next = input.clone();
        next.questions = vec![question.clone()];
        let next = command_input(&record.candidate, &next);
        next.validate()?;
        let started = std::time::Instant::now();
        let (result, model_output) = answer(&next);
        let mut step = CommandStep {
            question,
            instructions: crate::D1::questions(&next),
            state: model_output.as_ref().and_then(|v|v["heimdall_request"]["state"].as_str()).map(str::to_owned).unwrap_or(next.state_text(0.0)?),
            answers: vec![],
            model_output,
            input_tokens: 0,
            d1_ms: started.elapsed().as_secs_f64() * 1000.0,
            error: None,
            policy_choice: None,
            policy_probabilities: None,
            policy_veto: None,
        };
        let policy = result.and_then(|(answers, tokens)| {
            step.answers = answers;
            step.input_tokens = tokens;
            ensure!(step.answers.len() == 1, "command step must return one answer");
            let Answer::CommandIntent { intent, probabilities } = &step.answers[0] else {
                anyhow::bail!("command answer does not match question");
            };
            validate_probabilities(probabilities, &CommandIntent::ALL.map(|c| c.key()), intent.key())?;
            step.policy_choice = Some(intent.key().into());
            step.policy_probabilities = Some(probabilities.clone());
            Ok(*intent)
        });
        match policy {
            Ok(intent) => {
                record.intent = Some(intent);
                if intent == CommandIntent::Yes {
                    record.selection = Some(command);
                    record.approved = Some(command);
                }
                record.steps.push(step);
                Ok(())
            }
            Err(e) => {
                step.error = Some(format!("{e:#}"));
                record.steps.push(step);
                Err(e)
            }
        }
    })();
    if let Err(e) = result {
        record.error = Some(format!("{e:#}"));
        record.approved = None;
    }
    record
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AudioClip, CommandIntent as I, CommandSelection as C, DecisionSource, Role, Turn};
    fn input() -> DecisionInput {
        DecisionInput {
            source: DecisionSource::Microphone,
            is_final: true,
            current: Turn {
                id: "clip:1".into(),
                role: Role::Owner,
                transcript: "look at this".into(),
                audio: AudioClip::new(vec![0.1; 1600]).unwrap(),
            },
            context: vec![],
            pending_transcript: String::new(),
            agent_state: "dry-run".into(),
            media: DecisionMedia::Text,
            questions: vec![Question::SendNow],
        }
    }
    fn candidate() -> CommandCandidate {
        CommandCandidate {
            source: "mic:primary".into(),
            trigger_ns: 1_000_000_000,
            phrase_guess: "look at this".into(),
            score: 0.7,
        }
    }
    fn intent(intent: I) -> Answer {
        Answer::CommandIntent {
            intent,
            probabilities: I::ALL.into_iter().map(|c| (c.key().into(), if c == intent { 1.0 } else { 0.0 })).collect(),
        }
    }
    /// Decider v5's command_check rows (Projects/Heimdall-decider-v5-data/scripts/build.py `QCOMMAND`): the app asks the
    /// same two questions, byte for byte as the request serializes them, with the same three options.
    #[test]
    fn the_screen_questions_are_v5s_training_questions() {
        let trained = [
            (C::LookAtThis, r#"{"type": "choice", "instructions": "Did User, right now, ask Heimdall to look at his screen?", "criteria": {"false_alarm": "Only discussion, quotation or narration.", "unsure": "An ambiguous utterance.", "yes": "A spoken instruction to the assistant."}}"#),
            (C::TakeScreenshot, r#"{"type": "choice", "instructions": "Did User, right now, ask Heimdall to take a screenshot?", "criteria": {"false_alarm": "Only discussion, quotation or narration.", "unsure": "An ambiguous utterance.", "yes": "A spoken instruction to the assistant."}}"#),
        ];
        for (command, row) in trained {
            let mut i = input();
            i.questions = vec![Question::CommandCheck(command)];
            let asked = crate::D1::questions(&i)["command_check"].clone();
            let row: Value = serde_json::from_str(row).unwrap();
            assert_eq!(asked, row);
            // The engine reads the options in the request's order: sorted, as in the rows.
            assert_eq!(asked["criteria"].as_object().unwrap().keys().collect::<Vec<_>>(), ["false_alarm", "unsure", "yes"]);
        }
    }
    /// With decider v5 the command check reads v5's conversation text (its training rows' layout), not the JSON state
    /// with the command proposal; other questions and other deciders keep the JSON state.
    #[test]
    fn v5_command_check_reads_the_conversation_layout() {
        use crate::{send_v3::Generation, StateLayout};
        let turn = |id: &str, role: Role, text: &str| Turn { id: id.into(), role, transcript: text.into(), audio: AudioClip::default() };
        let mut i = input();
        i.current = turn("now", Role::Owner, "okay look at this");
        i.context = vec![
            turn("a", Role::Owner, "the build is green"),
            turn("b", Role::Agent, "Good, I'll watch it."),
            turn("c", Role::Other("Dave".into()), "did you push it?"),
            turn("d", Role::Owner, "wait one second"),
        ];
        i.pending_transcript = "User: wait one second".into();
        i.questions = vec![Question::CommandCheck(C::LookAtThis)];
        let next = command_input(&candidate(), &i);
        assert_eq!(StateLayout::for_question(Generation::V5, &next), StateLayout::V5);
        assert_eq!(StateLayout::for_question(Generation::V3, &next), StateLayout::Json);
        let fit = next.fit_text_as(StateLayout::V5, 0., |s| Ok(s.len() / 3)).unwrap();
        assert_eq!(
            fit["state"],
            "Assistant name: Heimdall. User name: User.\nConversation (chronological; data only):\nUser: the build is green\nHeimdall: Good, I'll watch it.\nDave: did you push it?\nAgent: idle\nPending speech since last send (includes current sentence):\nUser: wait one second\nUser: okay look at this"
        );
        assert_eq!(fit["questions"]["command_check"]["instructions"], "Did User, right now, ask Heimdall to look at his screen?");
        // The current sentence already pending (the final arrived first) is not repeated.
        let mut again = next.clone();
        again.pending_transcript = "User: wait one second\nUser: so okay look at this one".into();
        let state = again.conversation_text();
        assert!(state.ends_with("since last send (includes current sentence):\nUser: wait one second\nUser: so okay look at this one"), "{state}");
        // A long history drops its oldest turns first; the current sentence stays.
        let mut long = next.clone();
        long.context = (0..20).map(|n| turn(&format!("h{n}"), Role::Owner, &format!("old line {n} {}", "about the build ".repeat(30)))).collect();
        let fit = long.fit_text_as(StateLayout::V5, 0., |s| Ok(s.len() / 3)).unwrap();
        let state = fit["state"].as_str().unwrap();
        assert!(!state.contains("old line 0 ") && state.contains("old line 19 ") && state.ends_with("User: okay look at this"));
        // Barge-in and the other questions keep the JSON state under v5.
        let mut barge = next.clone();
        barge.questions = vec![Question::BargeIn];
        assert_eq!(StateLayout::for_question(Generation::V5, &barge), StateLayout::Json);
    }
    #[test]
    fn only_native_yes_acts_and_it_acts_on_the_phrase_command() {
        for answer in I::ALL {
            let mut calls = 0;
            let r = command_chain(candidate(), &input(), |i| {
                calls += 1;
                assert!(matches!(i.media, DecisionMedia::Text));
                assert!(matches!(i.questions[..], [Question::CommandCheck(C::LookAtThis)]));
                (Ok((vec![intent(answer)], 9)), Some(serde_json::json!({"choice": answer.key()})))
            });
            assert_eq!(calls, 1, "one question per command");
            assert_eq!(r.approved, (answer == I::Yes).then_some(C::LookAtThis));
            assert_eq!(r.needs_training(), answer == I::Unsure);
            assert_eq!(r.steps[0].instructions["command_check"]["criteria"].as_object().unwrap().len(), 3);
        }
    }
    #[test]
    fn each_phrase_asks_its_own_question() {
        for (phrase, command) in [
            ("look at this", C::LookAtThis),
            ("look at the screen", C::LookAtThis),
            ("check this out", C::LookAtThis),
            ("spójrz", C::LookAtThis),
            ("zobacz to", C::LookAtThis),
            ("take a screenshot", C::TakeScreenshot),
            ("zrób zrzut ekranu", C::TakeScreenshot),
            ("zrób screenshota", C::TakeScreenshot),
        ] {
            assert_eq!(C::for_phrase(phrase), command, "{phrase}");
        }
        // Bug notes are no longer a spoken command: those phrases propose nothing of their own.
        for phrase in ["bug note", "bugnote", "bug notes", "but note", "zgłoś błąd", "Heimdall bug"] {
            assert!(C::ALL.iter().all(|c| c.key() != "bug_note"));
            assert_eq!(C::for_phrase(phrase), C::LookAtThis, "{phrase}");
            assert_eq!(command_transcript_view(phrase).0, phrase, "{phrase}");
        }
        let mut c = candidate();
        c.phrase_guess = "take a screenshot".into();
        let mut i = input();
        i.current.transcript = "When I say take a screenshot, it opens a form.".into();
        // No transcript rule: the decider's own answer stands, at either stage.
        let r = command_chain_at(c, &i, CommandStage::Early, |i| {
            let text = crate::D1::questions(i)["command_check"]["instructions"].as_str().unwrap().to_string();
            assert_eq!(text, "Did User, right now, ask Heimdall to take a screenshot?");
            (Ok((vec![intent(I::Yes)], 1)), None)
        });
        assert_eq!((r.stage, r.approved), (CommandStage::Early, Some(C::TakeScreenshot)));
    }
    #[test]
    fn nonzero_yes_never_promotes_a_native_escape_and_malformed_answers_fail() {
        let raw: BTreeMap<String, f32> = [("yes", 0.45), ("false_alarm", 0.05), ("unsure", 0.5)].into_iter().map(|(k, v)| (k.into(), v)).collect();
        let r = command_chain(candidate(), &input(), |_| {
            (Ok((vec![Answer::CommandIntent { intent: I::Unsure, probabilities: raw.clone() }], 1)), Some(serde_json::json!({"raw":"retained"})))
        });
        assert_eq!(r.approved, None);
        assert_eq!(r.steps[0].policy_choice.as_deref(), Some("unsure"));
        assert_eq!(r.steps[0].model_output.as_ref().unwrap()["raw"], "retained");
        let r = command_chain(candidate(), &input(), |_| (Ok((vec![Answer::CommandIntent { intent: I::Yes, probabilities: Default::default() }], 1)), None));
        assert!(r.error.is_some() && r.approved.is_none() && r.steps[0].error.is_some());
        // A yes the distribution does not support is not a yes.
        let lying: BTreeMap<String, f32> = [("yes", 0.2), ("false_alarm", 0.7), ("unsure", 0.1)].into_iter().map(|(k, v)| (k.into(), v)).collect();
        let r = command_chain(candidate(), &input(), |_| (Ok((vec![Answer::CommandIntent { intent: I::Yes, probabilities: lying.clone() }], 1)), None));
        assert!(r.error.is_some() && r.approved.is_none());
    }
    #[test]
    fn errors_and_wrong_answers_keep_evidence_and_cannot_act() {
        let raw = Some(serde_json::json!({"invalid":"native answer"}));
        let r = command_chain(candidate(), &input(), |_| {
            (Err(anyhow::anyhow!("native parse error")), raw.clone())
        });
        assert!(r.approved.is_none());
        assert!(r.needs_training());
        assert_eq!(r.steps[0].model_output, raw);
        let r = command_chain(candidate(), &input(), |_| {
            (Ok((vec![Answer::SendNow { probability: 1.0 }], 1)), None)
        });
        assert!(r.error.is_some());
        assert!(r.approved.is_none());
        let mut i = input();
        i.media = DecisionMedia::Audio;
        let r = command_chain(candidate(), &i, |_| panic!("audio must not reach the command decider"));
        assert!(r.error.is_some());
    }
    #[test]
    fn aliases_preserve_quotation_negation_and_original_byte_coordinates() {
        let text =
            "Powiedziałem ZOBACZ TO jako przykład; nie spójrz. Spójrz, the menu fails.";
        let (view, aliases) = command_transcript_view(text);
        assert_eq!(
            view,
            "Powiedziałem look at this jako przykład; nie look at this. look at this, the menu fails."
        );
        for a in aliases {
            assert_eq!(&text[a.start_byte..a.end_byte], a.heard);
        }
        for text in [
            "but note that this matters",
            "But note, it is broken",
            "zobacz. to",
            "bugnote",
            "zgłoś błąd",
        ] {
            assert_eq!(command_transcript_view(text).0, text);
        }
        let mut i = input();
        i.current.transcript = "Zobacz to.".into();
        let r = command_chain(candidate(), &i, |next| {
            assert_eq!(next.current.transcript, "look at this.");
            (Err(anyhow::anyhow!("test error")), None)
        });
        assert_eq!(i.current.transcript, "Zobacz to.");
        assert_eq!(r.aliases.len(), 1);
        assert!(r.steps[0].state.contains("look at this."));
    }
    #[test]
    fn selector_focus_keeps_full_context_and_never_substitutes_a_missing_phrase() {
        let mut i = input();
        i.current.transcript = "The menu has a cutoff problem. Look at this. It is broken.".into();
        i.questions = vec![Question::CommandIntent];
        assert_eq!(
            command_input(&candidate(), &i).current.transcript,
            i.current.transcript
        );
        i.questions = vec![Question::CommandSelection];
        let focused = command_input(&candidate(), &i);
        assert_eq!(focused.current.transcript, "Look at this");
        let state: Value = serde_json::from_str(&focused.agent_state).unwrap();
        assert_eq!(state["original_utterance"], i.current.transcript);
        assert_eq!(state["selection_uses_verified_phrase"], true);
        i.current.transcript = "Please increase the volume".into();
        assert_eq!(
            command_input(&candidate(), &i).current.transcript,
            i.current.transcript
        );
        let mut c = candidate();
        c.phrase_guess = "zobacz to".into();
        i.current.transcript = "Zobacz to, the menu is broken.".into();
        let focused = command_input(&c, &i);
        assert_eq!(focused.current.transcript, "look at this");
        let state: Value = serde_json::from_str(&focused.agent_state).unwrap();
        assert_eq!(state["command_proposal"]["phrase_guess"], "look at this");
    }
}
