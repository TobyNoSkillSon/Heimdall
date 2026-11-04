//! Local typed decisions. Every utterance is archived regardless of addressedness.
mod archive;
mod command;
pub use command::*;
mod d1;
pub mod send;
// Personalise (26 Oct): the assistant's and the user's names in the decider's context and the wake spotter.
pub mod names;
pub mod send_v3;
/// Decider V5's suggest yes/no on every line (29 Oct).
pub mod suggest;
/// Decider V5's precision (16-bit, 8-bit, 4-bit): the setting, folders, sizes and the missing-folder fallback.
pub mod precision;
#[path = "../../speak/src/gpu_recovery.rs"]
pub mod gpu_recovery;
mod thought;
mod timing;
pub use timing::DecisionSource;
pub mod reply;
pub mod barge;
use anyhow::{Context, Result, ensure};
pub use archive::{ArchiveSink, TrainingArchive};
pub use d1::{D1, DEFAULT_LIBRARY, DEFAULT_MODEL, LLAMA_LIBRARY};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Instant,
};
pub use thought::PendingThought;

pub const SAMPLE_RATE: u32 = 16_000;
/// Byte bounds `DecisionInput::validate` enforces on text state. Callers holding
/// longer text keep its newest part with `newest_suffix`; the 2000-token fit
/// never reaches further back than this.
pub const MAX_TRANSCRIPT_BYTES: usize = 32768;
pub const MAX_PENDING_BYTES: usize = 65536;
pub const MAX_AGENT_STATE_BYTES: usize = 32768;
/// The newest part of `text` within `max` bytes, cut on a character boundary.
pub fn newest_suffix(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut cut = text.len() - max;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    &text[cut..]
}
/// The input is larger than the decider's bounds, so the question was not asked.
/// This is a refusal of that one input, not a model fault.
#[derive(Debug)]
pub struct Refused(pub &'static str);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Refused {}
/// Whether `e` is a size refusal (`Refused`) rather than a runtime failure.
pub fn is_refused(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<Refused>().is_some())
}
pub const MAX_CONTEXT: usize = 20;
pub const MAX_AUDIO: usize = 30 * SAMPLE_RATE as usize;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", content = "speaker", rename_all = "snake_case")]
pub enum Role {
    Owner,
    Other(String),
    Agent,
}
/// A final utterance's waveform, mono f32 [-1,1]. No microphone access in this crate.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AudioClip {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
}
impl AudioClip {
    pub fn new(samples: Vec<f32>) -> Result<Self> {
        let clip = Self {
            samples,
            sample_rate: SAMPLE_RATE,
        };
        clip.validate()?;
        Ok(clip)
    }
    pub fn from_wav(path: impl AsRef<Path>) -> Result<Self> {
        let mut r = hound::WavReader::open(path)?;
        let s = r.spec();
        ensure!(
            s.channels == 1 && s.sample_rate == SAMPLE_RATE,
            "expected mono 16 kHz WAV"
        );
        let samples = if s.sample_format == hound::SampleFormat::Float {
            r.samples::<f32>()
                .collect::<std::result::Result<Vec<_>, _>>()?
        } else {
            let scale = 2_f32.powi(s.bits_per_sample as i32 - 1);
            r.samples::<i32>()
                .map(|v| v.map(|x| x as f32 / scale))
                .collect::<std::result::Result<Vec<_>, _>>()?
        };
        Self::new(samples)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.sample_rate == SAMPLE_RATE, "audio must be 16 kHz");
        ensure!(
            !self.samples.is_empty() && self.samples.len() <= MAX_AUDIO,
            "audio must contain 1..480000 samples; supply an explicitly bounded recent audio tail"
        );
        ensure!(
            self.samples
                .iter()
                .all(|x| x.is_finite() && (-1.0..=1.0).contains(x)),
            "audio must be finite PCM in [-1,1]"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub role: Role,
    pub transcript: String,
    /// Legacy archive/probe waveform; never passed to d1. Live text inputs omit it.
    #[serde(default)]
    pub audio: AudioClip,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Image {
    pub mime: String,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
/// Legacy archive media tags. Production d1 requests always contain text only.
pub enum DecisionMedia {
    Audio,
    Image(Image),
    Text,
}
/// Command set stays closed. Exact screenshot phrases should be handled in code first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixedCommand {
    None,
    TakeScreenshot,
    LookAtThis,
    OpenCandidate,
    StopSpeaking,
    CancelTask,
}
impl FixedCommand {
    pub const ALL: [Self; 6] = [
        Self::None,
        Self::TakeScreenshot,
        Self::LookAtThis,
        Self::OpenCandidate,
        Self::StopSpeaking,
        Self::CancelTask,
    ];
    pub fn key(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::TakeScreenshot => "take_screenshot",
            Self::LookAtThis => "look_at_this",
            Self::OpenCandidate => "open_candidate",
            Self::StopSpeaking => "stop_speaking",
            Self::CancelTask => "cancel_task",
        }
    }
    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::None => "No fixed command is requested",
            Self::TakeScreenshot => "Capture the current screen",
            Self::LookAtThis => "Inspect the screen at the utterance timestamp",
            Self::OpenCandidate => "Open one of the supplied links or items",
            Self::StopSpeaking => "Stop the agent's spoken reply",
            Self::CancelTask => "Cancel the current agent task",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub description: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "question", content = "candidates", rename_all = "snake_case")]
pub enum Question {
    SendNow,
    /// The generic "is the user requesting an action?" (kept for archived records and parity checks).
    CommandIntent,
    /// One command-specific question at the trigger: did the user, right now, ask for this command?
    CommandCheck(crate::CommandSelection),
    CommandSelection,
    FixedCommand,
    PickCandidate(Vec<Candidate>),
    /// End of the user's turn: which voice-bank line to say now, or `none`. See `reply`.
    ReplyLine(Vec<Candidate>),
    /// Heimdall is speaking and the user talked over it: stop or continue. See `barge`.
    BargeIn,
}
impl Question {
    pub fn key(&self) -> &'static str {
        match self {
            Self::SendNow => "send_now",
            Self::CommandIntent => "command_intent",
            Self::CommandCheck(_) => "command_check",
            Self::CommandSelection => "command_selection",
            Self::FixedCommand => "fixed_command",
            Self::PickCandidate(_) => "pick_candidate",
            Self::ReplyLine(_) => "reply_line",
            Self::BargeIn => "barge_in",
        }
    }
    pub(crate) fn validate(&self) -> Result<()> {
        if let Self::CommandCheck(c) = self {
            ensure!(c.listed(), "a command check names a listed command");
        }
        if let Self::ReplyLine(c) = self {
            ensure!(c.iter().any(|v| v.id == reply::NONE), "reply line choice needs a none option");
        }
        if let Self::PickCandidate(c) | Self::ReplyLine(c) = self {
            ensure!(
                (2..=255).contains(&c.len()),
                "candidate choice requires 2..255 options"
            );
            let ids: BTreeSet<_> = c.iter().map(|v| &v.id).collect();
            ensure!(
                ids.len() == c.len()
                    && c.iter().all(|v| !v.id.trim().is_empty()
                        && v.id.len() <= 256
                        && !v.description.trim().is_empty()
                        && v.description.len() <= 2048),
                "candidate IDs must be unique/nonempty and descriptions bounded"
            );
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecisionInput {
    #[serde(default)]
    pub source: DecisionSource,
    #[serde(default)]
    pub is_final: bool,
    pub current: Turn,
    /// Chronological prior text turns, at most 20.
    pub context: Vec<Turn>,
    pub pending_transcript: String,
    pub agent_state: String,
    pub media: DecisionMedia,
    pub questions: Vec<Question>,
}
impl DecisionInput {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.current.role != Role::Agent,
            "agent speech is context, not a trigger"
        );
        ensure!(self.context.len() <= MAX_CONTEXT, "at most 20 prior turns");
        let mut ids = BTreeSet::new();
        for t in self.context.iter().chain(std::iter::once(&self.current)) {
            ensure!(
                !t.id.is_empty() && ids.insert(&t.id),
                "turn IDs must be unique/nonempty"
            );
            ensure!(
                t.transcript.len() <= MAX_TRANSCRIPT_BYTES,
                Refused("transcript exceeds 32 KiB")
            );
        }
        ensure!(
            self.pending_transcript.len() <= MAX_PENDING_BYTES
                && self.agent_state.len() <= MAX_AGENT_STATE_BYTES,
            Refused("state exceeds bounded input size")
        );
        ensure!(
            !self.questions.is_empty() && self.questions.len() <= 3,
            "one to three fixed questions required"
        );
        let mut qids = BTreeSet::new();
        for q in &self.questions {
            q.validate()?;
            ensure!(qids.insert(q.key()), "duplicate question");
        }
        if let DecisionMedia::Image(i) = &self.media {
            ensure!(
                matches!(i.mime.as_str(), "image/png" | "image/jpeg")
                    && !i.bytes.is_empty()
                    && i.bytes.len() <= 16 * 1024 * 1024,
                "image must be PNG/JPEG <=16 MiB"
            );
        }
        Ok(())
    }
    /// Upper bound for watchdog sizing, without tokenizing on the live loop.
    /// UTF-8 byte count bounds tokenizer length; fitted requests never exceed 2k.
    pub fn token_bound(&self) -> usize {
        (self.state_text(0.).map_or(0, |s| s.len()) + D1::questions(self).to_string().len() + 128)
            .min(send::WINDOW)
    }
    /// Fit every text question with the actual native tokenizer. Evict oldest
    /// turns first, then old pending text and agent state. The complete current
    /// utterance is retained or the request is explicitly refused.
    pub fn fit_text(
        &self,
        p: f32,
        tokens: impl FnMut(&str) -> Result<usize>,
    ) -> Result<serde_json::Value> {
        self.fit_text_as(StateLayout::Json, p, tokens)
    }
    /// `fit_text` with the state written in `layout`.
    pub fn fit_text_as(
        &self,
        layout: StateLayout,
        p: f32,
        mut tokens: impl FnMut(&str) -> Result<usize>,
    ) -> Result<serde_json::Value> {
        self.validate()?;
        ensure!(
            p.is_finite() && (0.0..=1.0).contains(&p),
            "invalid addressedness"
        );
        let questions = D1::questions(self);
        let overhead = tokens(&questions.to_string())? + 128;
        ensure!(
            overhead < send::WINDOW,
            "question/options exceed 2000-token window"
        );
        let budget = send::WINDOW - overhead;
        // Text only: don't clone obsolete PCM or images into the fitting loop.
        let mut fitted = Self {
            source: self.source,
            is_final: self.is_final,
            current: Turn {
                id: self.current.id.clone(),
                role: self.current.role.clone(),
                transcript: self.current.transcript.clone(),
                audio: AudioClip::default(),
            },
            context: self
                .context
                .iter()
                .map(|t| Turn {
                    id: t.id.clone(),
                    role: t.role.clone(),
                    transcript: t.transcript.clone(),
                    audio: AudioClip::default(),
                })
                .collect(),
            pending_transcript: self.pending_transcript.clone(),
            agent_state: self.agent_state.clone(),
            media: DecisionMedia::Text,
            questions: self.questions.clone(),
        };
        let mut state = fitted.state_as(layout, p)?;
        if tokens(&state)? > budget {
            let (mut lo, mut hi) = (0, fitted.context.len());
            while lo < hi {
                let mid = (lo + hi) / 2;
                let mut probe = fitted.clone();
                probe.context.drain(..mid);
                if tokens(&probe.state_as(layout, p)?)? > budget {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            fitted.context.drain(..lo);
            state = fitted.state_as(layout, p)?;
        }
        // Preserve newest pending speech; serializing each suffix before counting
        // handles JSON escaping and Unicode exactly as inference sees them.
        if tokens(&state)? > budget {
            let pending = std::mem::take(&mut fitted.pending_transcript);
            let boundaries: Vec<_> = pending
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(pending.len()))
                .collect();
            let (mut lo, mut hi) = (0, boundaries.len() - 1);
            while lo < hi {
                let mid = (lo + hi) / 2;
                fitted.pending_transcript = pending[boundaries[mid]..].into();
                if tokens(&fitted.state_as(layout, p)?)? > budget {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            fitted.pending_transcript = pending[boundaries[lo]..].into();
            state = fitted.state_as(layout, p)?;
        }
        if tokens(&state)? > budget {
            let agent = std::mem::take(&mut fitted.agent_state);
            let boundaries: Vec<_> = agent
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(agent.len()))
                .collect();
            let (mut lo, mut hi) = (0, boundaries.len() - 1);
            while lo < hi {
                let mid = (lo + hi) / 2;
                fitted.agent_state = agent[boundaries[mid]..].into();
                if tokens(&fitted.state_as(layout, p)?)? > budget {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            fitted.agent_state = agent[boundaries[lo]..].into();
            state = fitted.state_as(layout, p)?;
        }
        ensure!(
            tokens(&state)? <= budget,
            Refused("Current utterance and question exceed d1 token budget; decision refused (2000-token window)")
        );
        Ok(serde_json::json!({"state":state,"questions":questions}))
    }
    /// The unfitted state in `layout`.
    pub fn state_as(&self, layout: StateLayout, p: f32) -> Result<String> {
        match layout {
            StateLayout::Json => self.state_text(p),
            StateLayout::V5 => Ok(self.conversation_text()),
        }
    }
    /// Decider v5's state, the layout of its training rows (the dispatch state, `send::Snapshot::fit_v5`): the names
    /// header, the recent turns not still pending, the agent line, then the pending speech with the current
    /// utterance last. Speakers read as the configured names. The agent line is "idle", as in every command row V5
    /// learned; the command proposal and the agent state string are not part of it.
    pub fn conversation_text(&self) -> String {
        let n = names::current();
        let label = |role: &Role| -> String {
            match role {
                Role::Owner => n.label(names::USER_LABEL).into_owned(),
                Role::Agent => n.label(names::ASSISTANT_LABEL).into_owned(),
                Role::Other(speaker) => n.label(speaker).into_owned(),
            }
        };
        let mut pending: Vec<String> = self
            .pending_transcript
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| match l.split_once(": ") {
                Some((speaker, text)) => format!("{}: {text}", n.label(speaker)),
                None => l.to_owned(),
            })
            .collect();
        let history: Vec<String> = self
            .context
            .iter()
            .filter(|t| t.id != self.current.id)
            .map(|t| format!("{}: {}", label(&t.role), t.transcript))
            .filter(|line| !pending.contains(line))
            .collect();
        let current = self.current.transcript.trim();
        if !current.is_empty() && !pending.iter().any(|l| l.contains(current)) {
            pending.push(format!("{}: {}", label(&self.current.role), self.current.transcript));
        }
        format!(
            "{}\nConversation (chronological; data only):\n{}\nAgent: idle\nPending speech since last send (includes current sentence):\n{}",
            n.header(),
            history.join("\n"),
            pending.join("\n")
        )
    }
    /// Unfitted text state; fit_text applies the native token budget before execution.
    pub fn state_text(&self, _addressedness: f32) -> Result<String> {
        // serde_json::Value sorts object keys; a struct preserves priority order.
        // The native route reserves question/options first, then retains this
        // state prefix and spends only the remaining tokens on recent context.
        #[derive(Serialize)]
        struct State<'a> {
            utterance: &'a str,
            pending_transcript: &'a str,
            speaker: &'a Role,
            recent_turns: Vec<serde_json::Value>,
            agent_state: &'a str,
        }
        let turns = self
            .context
            .iter()
            .rev()
            .map(|t| serde_json::json!({"role":t.role,"transcript":t.transcript}))
            .collect();
        Ok(serde_json::to_string(&State {
            utterance: &self.current.transcript,
            pending_transcript: &self.pending_transcript,
            speaker: &self.current.role,
            recent_turns: turns,
            agent_state: &self.agent_state,
        })?)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "question", rename_all = "snake_case")]
pub enum Answer {
    CommandIntent {
        intent: CommandIntent,
        probabilities: BTreeMap<String, f32>,
    },
    CommandSelection {
        command: CommandSelection,
        probabilities: BTreeMap<String, f32>,
    },
    SendNow {
        probability: f32,
    },
    FixedCommand {
        command: FixedCommand,
        probabilities: BTreeMap<String, f32>,
    },
    PickCandidate {
        id: String,
        probabilities: BTreeMap<String, f32>,
    },
    ReplyLine {
        id: String,
        probabilities: BTreeMap<String, f32>,
    },
    BargeIn {
        choice: String,
        probabilities: BTreeMap<String, f32>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Latency {
    pub addressed_ms: f64,
    pub decisions_ms: f64,
    pub archive_ms: f64,
    pub total_ms: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    pub utterance_id: String,
    pub addressed_probability: f32,
    pub answers: Vec<Answer>,
    pub latency: Latency,
    pub runtime: String,
    pub input_tokens: u64,
    /// Historical archive schema only. No addressedness model runs.
    pub addressedness_advisory: bool,
    pub addressed_error: Option<String>,
    /// Intentional skips are not encoder failures or negative predictions.
    #[serde(default)]
    pub addressed_skip_reason: Option<String>,
    pub raw_send_probability: Option<f32>,

}
/// Called once on each final human utterance. Errors are explicit; there is no heuristic fallback.
pub trait Decider {
    fn decide(&mut self, input: &DecisionInput) -> Result<Decision>;
}
/// How a question's state is written for the loaded decider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateLayout {
    /// The JSON state (`DecisionInput::state_text`) the 600M deciders read for every question.
    Json,
    /// Decider v5's conversation text (`DecisionInput::conversation_text`), for the questions v5 learned on it.
    V5,
}
impl StateLayout {
    /// V5's layout for its command check (the look-at-this and screenshot questions of its training rows); the JSON
    /// state for every other question and decider. v5 never learned the other questions (barge-in, reply line,
    /// fixed command, candidates), so they keep the state they had.
    pub fn for_question(generation: send_v3::Generation, input: &DecisionInput) -> Self {
        match (generation, input.questions.as_slice()) {
            (send_v3::Generation::V5, [Question::CommandCheck(_)]) => Self::V5,
            _ => Self::Json,
        }
    }
}
pub struct LocalDecider {
    d1: D1,
    archive: TrainingArchive,
}
impl LocalDecider {
    pub fn set_archive_sink(&mut self, sink: ArchiveSink) { self.archive.set_sink(sink); }
    /// The 16-bit checkpoint (`d1::DEFAULT_MODEL`, Decider V6): parity tests and the replay tools. The app loads its
    /// chosen version and precision (`load_pick`).
    pub fn load(models: impl AsRef<Path>, archive: impl AsRef<Path>) -> Result<Self> {
        Self::load_folder(models.as_ref(), Path::new(d1::DEFAULT_MODEL), archive.as_ref())
    }
    /// One decider folder (`precision::Pick`, as given: the caller resolves a missing one, `precision::resolve`).
    pub fn load_pick(models: impl AsRef<Path>, archive: impl AsRef<Path>, pick: precision::Pick) -> Result<Self> {
        Self::load_folder(models.as_ref(), Path::new(pick.folder()), archive.as_ref())
    }
    fn load_folder(m: &Path, folder: &Path, archive: &Path) -> Result<Self> {
        let library = std::env::var_os("HEIMDALL_D1_LIBRARY").map(std::path::PathBuf::from).unwrap_or_else(||m.join(d1::DEFAULT_LIBRARY));
        Ok(Self { d1:D1::load(library, m.join(folder))?, archive:TrainingArchive::open(archive)? })
    }
    pub fn last_send_request(&self)->Option<&send::Fitted>{self.d1.last_send_request.as_ref()}
    pub fn send_decision(&mut self, snapshot:&send::Snapshot) -> Result<(send::Fitted,send::Prediction)> {
        self.d1.send_decision(snapshot)
    }
    pub fn send_decision_interruptible(&mut self,snapshot:&send::Snapshot,urgent:&std::sync::atomic::AtomicUsize)->Result<(send::Fitted,send::Prediction)> {
        self.d1.send_decision_interruptible(snapshot,urgent)
    }
    /// Decider V5's suggest question (`suggest`); errors on deciders before V5.
    pub fn suggest_decision_interruptible(&mut self,snapshot:&send::Snapshot,urgent:&std::sync::atomic::AtomicUsize)->Result<(send::Fitted,suggest::Suggestion)> {
        self.d1.suggest_decision_interruptible(snapshot,urgent)
    }
    /// Whether the loaded decider learned the suggest question (V5 and later).
    pub fn suggests(&self) -> bool {
        self.d1.generation() == send_v3::Generation::V5
    }
    pub fn command_runtime(&self) -> &str {
        self.d1.version()
    }
    /// Command-only calls do not alter the rolling send conversation.
    pub fn command_decision(
        &mut self,
        candidate: CommandCandidate,
        input: &DecisionInput,
    ) -> CommandDecision {
        self.command_decision_at(candidate, input, CommandStage::Final)
    }
    /// The same chain at `stage`: early (audio at detection, no transcript veto) or final.
    pub fn command_decision_at(
        &mut self,
        candidate: CommandCandidate,
        input: &DecisionInput,
        stage: CommandStage,
    ) -> CommandDecision {
        command_chain_at(candidate, input, stage, |next| self.command_answer(next))
    }
    /// Command-chain stage (including supplied-item selection), with raw evidence and no send side effects.
    pub fn command_answer(
        &mut self,
        input: &DecisionInput,
    ) -> (Result<(Vec<Answer>, u64)>, Option<serde_json::Value>) {
        let result = (|| {
            ensure!(
                matches!(input.media, DecisionMedia::Text),
                "command decisions read text only"
            );
            ensure!(
                input.questions.len() == 1
                    && matches!(
                        input.questions[0],
                        Question::CommandIntent
                            | Question::CommandCheck(_)
                            | Question::CommandSelection
                            | Question::PickCandidate(_)
                    ),
                "not a command-chain stage"
            );
            self.d1.answer(input, 0.0)
        })();
        (result, self.d1.last_output.clone())
    }
}
impl Decider for LocalDecider {
    fn decide(&mut self, input: &DecisionInput) -> Result<Decision> {
        input.validate()?;
        self.d1.last_output=None;
        let all=Instant::now();
        let result=(|| {
            let start=Instant::now();
            let (answers,input_tokens)=self.d1.answer(input,0.)?;
            Ok(Decision {utterance_id:input.current.id.clone(),addressed_probability:0.,answers,input_tokens,
                addressedness_advisory:true,addressed_error:None,addressed_skip_reason:Some("removed".into()),
                raw_send_probability:None,runtime:self.d1.version().into(),
                latency:Latency {addressed_ms:0.,decisions_ms:start.elapsed().as_secs_f64()*1000.,archive_ms:0.,total_ms:0.}})
        })();
        let model_outputs=serde_json::json!({"d1_output":self.d1.last_output});
        match result {
            Ok(mut decision) => {
                let start = Instant::now();
                self.archive
                    .append_with_outputs(
                        input,
                        Some(&decision),
                        None,
                        Some(&model_outputs),
                    )
                    .context("archive decision")?;
                decision.latency.archive_ms = start.elapsed().as_secs_f64() * 1000.;
                decision.latency.total_ms = all.elapsed().as_secs_f64() * 1000.;
                Ok(decision)
            }
            Err(e) => {
                self.archive.append_with_outputs(
                    input,
                    None,
                    Some(&format!("{e:#}")),
                    Some(&model_outputs),
                )?;
                Err(e)
            }
        }
    }
}
#[cfg(test)]
mod input_bound_tests {
    use super::*;
    #[test]
    fn newest_suffix_keeps_the_newest_bytes_on_a_char_boundary() {
        assert_eq!(newest_suffix("short", 64), "short");
        let text = format!("{}ząb end", "ł".repeat(40));
        for max in 1..text.len() {
            let tail = newest_suffix(&text, max);
            assert!(tail.len() <= max && text.ends_with(tail), "max {max}");
            assert!(tail.len() + 1 >= max, "drops at most one partial char");
        }
    }
    #[test]
    fn size_refusals_are_typed_and_keep_their_text() {
        let mut input = barge::input("bounded", "stop", "working").unwrap();
        input.questions = vec![Question::FixedCommand];
        input.pending_transcript = "p".repeat(MAX_PENDING_BYTES + 1);
        let e = input.validate().unwrap_err();
        assert_eq!(e.to_string(), "state exceeds bounded input size");
        assert!(is_refused(&e) && is_refused(&e.context("fixed command")));
        input.pending_transcript = newest_suffix(&input.pending_transcript, MAX_PENDING_BYTES).into();
        input.validate().unwrap();
        assert!(!is_refused(&anyhow::anyhow!("decider not loaded")));
    }
}
