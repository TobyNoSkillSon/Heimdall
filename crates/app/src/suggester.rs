//! The in-conversation suggester: while the user talks with someone, Heimdall sometimes offers one
//! card on the island ("Find the file", "Investigate it", "Not now") built from the last few minutes of talk. The card
//! is a question, never an action. The user's pick wakes the main agent with the suggestion, the talk it came from, what the
//! search found and his choice (`handoff`); only then does the agent start.
//!
//! - When: Decider V5's suggest yes/no on each line starts a pass (pipeline/suggest.rs `suggest_ask`, 29 Oct). The stub rule
//!   (`stub_suggest_now`, a cheap phrase rule over the user's own line) now runs only in the lab replay.
//! - Who answers: a background job on the background model (Haiku 5.5 by default), through `background_call`: the
//!   one-hour prompt cache, the $0.50 limit a run, the usage in the ledger. Its tools are read-only: `search_files`
//!   (Spotlight, under his home, Library caches out) and `search_timeline` (the archive). It has no web access.
//! - Output: `submit_suggestion` with `suggest: false` (silence, the usual answer) or one card of one to three questions.
//!   `verify` checks it in code. Every path it cites must come from a search in the same run, and a card that proposes
//!   contacting anyone is refused.
//! - Pacing (`Pacer`): one model call a minute, at most one card every three minutes, no repeat topic within two hours,
//!   no call or card while one is up, and two dismissals in a row pause suggestions for 15 minutes.
//! - Feedback: every decision is appended to `suggestions/log.jsonl` under Heimdall's data folder (`append`). A typed
//!   "bad suggestion" or "irrelevant" within five minutes of a card attaches to it (`feedback`). `export` turns the log
//!   into the decider's "suggest now" labels for v4.
//! - Prompt: `prompt/suggester.md`, and `prompt/public/suggester.md` in the public edition.

use crate::background_call::{self, Prompt, Tool};
use crate::plan_refresher::Endpoint;
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeSet, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The log and ledger label.
pub const JOB: &str = "suggester";
/// A short judgement with a few searches that the user waits on in a conversation: low, and never above it whatever
/// Background Tasks ▸ Model's level says (`background_call::capped_effort`; bug 2026-11-05T02-32-17: at "max" a pass
/// took 7-31 s).
pub const EFFORT: &str = "low";
pub const SUBMIT: &str = "submit_suggestion";
pub const SEARCH_FILES: &str = "search_files";
pub const SEARCH_TIMELINE: &str = "search_timeline";
/// Tool rounds per run: up to two searches, then the answer.
pub const MAX_ROUNDS: usize = 5;
/// Island card ids: `suggest:<uuid>`; the answer comes back with that request id.
pub const CARD_PREFIX: &str = "suggest:";
/// The main agent's notice for a pick: `suggest-pick:<uuid>`, classified as an answer (`notice.rs`).
pub const PICK_PREFIX: &str = "suggest-pick:";
/// How long a card waits on the island before it expires unanswered.
pub const CARD_MS: u64 = 5 * 60_000;
/// At most one card this far apart.
pub const GAP_MS: u64 = 3 * 60_000;
/// At most one model call this far apart, silent answers included.
pub const CALL_GAP_MS: u64 = 60_000;
/// A topic is not offered again within this time.
pub const TOPIC_MS: u64 = 2 * 3_600_000;
/// Two dismissals in a row pause suggestions this long.
pub const PAUSE_MS: u64 = 15 * 60_000;
/// Typed "bad suggestion" or "irrelevant" attaches to the last card within this time.
pub const FEEDBACK_MS: u64 = 5 * 60_000;
/// The talk the suggester sees: the last ten minutes, at most this many lines.
pub const WINDOW_MS: u64 = 10 * 60_000;
pub const WINDOW_LINES: usize = 60;
/// "Someone else talked" means within this time before the user's line.
const RECENT_MS: u64 = 5 * 60_000;
const MAX_FILES: usize = 12;
const MAX_HITS: usize = 8;

/// The system prompt (`prompt/suggester.md`; the public edition's copy has nothing of the user's).
#[cfg(not(feature = "public"))]
pub const SYSTEM: &str = include_str!("../prompt/suggester.md");
#[cfg(feature = "public")]
pub const SYSTEM: &str = include_str!("../prompt/public/suggester.md");

/// Searches a run may make; the rest are refused with a note, and the answer must come.
pub const MAX_SEARCHES: usize = 2;
/// A line of talk kept in the window: longer lines are cut (a pasted document does not fill the prompt).
const LINE_CHARS: usize = 400;

/// The phrases of the stub rule (`stub_suggest_now`), lowercase. Tightened after the replay (24 Oct): "something like",
/// "remember" and "look into" alone fired on everyday talk ("something like that was funny", "look into the screen").
const CUES: &[&str] = &[
    "i remember there was",
    "i remember a ",
    "don't remember what",
    "don't recall",
    "i forget",
    "there was something",
    "i think there was",
    "something like this on my",
    "something like that on my",
    "on my laptop",
    "on my mac",
    "somewhere on",
    "who was that",
    "who is that",
    "what was the name",
    "what's the name",
    "what was it called",
    "what's it called",
    "it was called",
    "i think it's called",
    "investigate",
    "look into that",
    "look into this",
];
/// Verbs that reach a person when a name or a pronoun follows at once ("Message Priya", "call them"). The noun uses
/// ("the messages from Priya", "a text file") pass: the next word is not a person.
const REACH: &[&str] = &["message", "email", "text", "call", "ping", "invite", "dm", "reply", "notify", "phone", "contact", "ring", "whatsapp", "sms", "imessage"];
/// Verbs that send something to a person within a few words ("send it to Priya", "forward it to them").
const SEND: &[&str] = &["send", "forward"];
/// Verbs that act on the Mac or the world beyond a look-up: a card may only offer to find, look up or investigate.
const OUTWARD: &[&str] = &["delete", "remove", "trash", "erase", "wipe", "buy", "purchase", "pay", "install", "uninstall", "publish", "upload", "post", "share", "transfer", "sell", "order", "unsubscribe"];
/// The answers that dismiss a card: "Not now" and the like, with nothing typed.
const DISMISS: [&str; 3] = ["Not now", "No", "Not this time"];

/// One finished line of talk the suggester sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub at_ms: u64,
    pub speaker: String,
    pub text: String,
    /// The user's own voice (his microphone).
    pub owner: bool,
}

/// The last `WINDOW_MS` of talk, at most `WINDOW_LINES` lines, oldest first.
#[derive(Clone, Debug, Default)]
pub struct Window {
    lines: VecDeque<Line>,
}

impl Window {
    pub fn push(&mut self, mut line: Line) {
        let cut = line.at_ms.saturating_sub(WINDOW_MS);
        line.text = line.text.chars().take(LINE_CHARS).collect();
        self.lines.push_back(line);
        while self.lines.front().is_some_and(|l| l.at_ms < cut) || self.lines.len() > WINDOW_LINES {
            self.lines.pop_front();
        }
    }
    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        self.lines.iter()
    }
    pub fn last(&self) -> Option<&Line> {
        self.lines.back()
    }
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
    /// Someone other than the user spoke within `RECENT_MS` before `now`.
    pub fn others_talking(&self, now: u64) -> bool {
        self.lines.iter().any(|l| !l.owner && l.at_ms + RECENT_MS >= now)
    }
    /// The talk as the model reads it: `[HH:MM Speaker] words`, one line each.
    pub fn transcript(&self) -> String {
        self.lines
            .iter()
            .map(|l| format!("[{} {}] {}", agent::clock::local_hm(l.at_ms), l.speaker, l.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Who is in the talk, in the order they first spoke (the user excluded).
    pub fn people(&self) -> Vec<String> {
        let mut names: Vec<String> = vec![];
        for l in self.lines.iter().filter(|l| !l.owner && !l.speaker.is_empty()) {
            if !names.contains(&l.speaker) {
                names.push(l.speaker.clone());
            }
        }
        names
    }
}

/// The decider's "suggest now", until v4 (the one seam). It fires on the user's own finished line, said while someone else
/// talked in the last five minutes, when the line is about something he half-remembers, wants looked up, or asks who
/// or what something was. It never fires on a line to Heimdall, which the main agent answers.
pub fn stub_suggest_now(w: &Window) -> bool {
    let Some(last) = w.last() else { return false };
    if !last.owner || !w.others_talking(last.at_ms) {
        return false;
    }
    let text = last.text.to_lowercase();
    let words: Vec<String> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect();
    !text.contains("heimdall") && !text.contains("heimdal") && !decide::names::current().mentioned(&words) && CUES.iter().any(|c| text.contains(c))
}

/// What a decision came to, for pacing and the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Shown,
    Picked,
    Dismissed,
    Ignored,
    Feedback,
}

impl Outcome {
    pub fn name(self) -> &'static str {
        match self {
            Outcome::Shown => "shown",
            Outcome::Picked => "picked",
            Outcome::Dismissed => "dismissed",
            Outcome::Ignored => "ignored",
            Outcome::Feedback => "feedback",
        }
    }
}

/// The pacing state: what may run now (`may_call`, `may_show`) and what happened (`called`, `shown`, `resolved`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pacer {
    last_call_ms: Option<u64>,
    last_card_ms: Option<u64>,
    topics: Vec<(String, u64)>,
    dismissed_in_row: u32,
    paused_until_ms: u64,
    card_open: bool,
}

impl Pacer {
    /// A model call may start: no card is up, the minute since the last call has passed, and no pause.
    pub fn may_call(&self, now: u64) -> bool {
        !self.card_open && now >= self.paused_until_ms && self.last_call_ms.is_none_or(|t| now.saturating_sub(t) >= CALL_GAP_MS)
    }
    /// A card may show: no card is up, the gap has passed, no pause, and the topic is new.
    pub fn may_show(&self, now: u64, topic: &str) -> bool {
        !self.card_open
            && now >= self.paused_until_ms
            && self.last_card_ms.is_none_or(|t| now.saturating_sub(t) >= GAP_MS)
            && !self.repeats(now, topic)
    }
    pub fn repeats(&self, now: u64, topic: &str) -> bool {
        self.topics.iter().any(|(t, at)| now.saturating_sub(*at) < TOPIC_MS && same_topic(t, topic))
    }
    pub fn called(&mut self, now: u64) {
        self.last_call_ms = Some(now);
    }
    pub fn shown(&mut self, now: u64, topic: &str) {
        self.last_card_ms = Some(now);
        self.card_open = true;
        self.topics.retain(|(_, at)| now.saturating_sub(*at) < TOPIC_MS);
        self.topics.push((topic.to_owned(), now));
    }
    /// The card left the island (answered, expired or replaced).
    pub fn closed(&mut self) {
        self.card_open = false;
    }
    /// A card's outcome. A pick clears the dismissal count; two dismissals in a row pause suggestions.
    pub fn resolved(&mut self, now: u64, outcome: Outcome) {
        match outcome {
            Outcome::Picked => self.dismissed_in_row = 0,
            Outcome::Dismissed => {
                self.dismissed_in_row += 1;
                if self.dismissed_in_row >= 2 {
                    self.paused_until_ms = now + PAUSE_MS;
                    self.dismissed_in_row = 0;
                }
            }
            Outcome::Shown | Outcome::Ignored | Outcome::Feedback => {}
        }
    }
    pub fn paused(&self, now: u64) -> bool {
        now < self.paused_until_ms
    }
}

/// Two topics are the same when their words mostly overlap (Jaccard 0.6 or more over words of three letters or more).
pub fn same_topic(a: &str, b: &str) -> bool {
    let words = |s: &str| -> BTreeSet<String> {
        s.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 3).map(str::to_lowercase).collect()
    };
    let (x, y) = (words(a), words(b));
    if x.is_empty() || y.is_empty() {
        return false;
    }
    let common = x.intersection(&y).count() as f64;
    common / x.union(&y).count() as f64 >= 0.6
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    pub text: String,
    pub options: Vec<String>,
    pub multi_select: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub path: String,
    pub note: String,
}

/// A checked card: its topic (for the no-repeat rule), its questions, what the search found and what the main agent
/// should know when the user picks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    pub topic: String,
    pub questions: Vec<Question>,
    pub found: Vec<Found>,
    pub handoff: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing to show, with the reason for the log.
    Silent(String),
    Card(Suggestion),
}

/// The `submit_suggestion` input: the one answer, last.
pub fn submit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "suggest": {"type": "boolean", "description": "false: show nothing. This is the usual answer."},
            "topic": {"type": "string", "description": "Two to six words naming the subject. Used to avoid repeats."},
            "questions": {
                "type": "array",
                "minItems": 1,
                "maxItems": 3,
                "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "One question User can answer with a click."},
                        "options": {
                            "type": "array",
                            "minItems": 2,
                            "maxItems": 4,
                            "items": {"type": "string", "description": "A short, plain outcome, at most 40 characters."}
                        },
                        "multi_select": {"type": "boolean"}
                    },
                    "required": ["text", "options", "multi_select"],
                    "additionalProperties": false
                }
            },
            "found": {
                "type": "array",
                "maxItems": 3,
                "items": {
                    "type": "object",
                    "properties": {"path": {"type": "string", "description": "Exactly as a search returned it."}, "note": {"type": "string"}},
                    "required": ["path", "note"],
                    "additionalProperties": false
                }
            },
            "handoff": {"type": "string", "description": "What Heimdall should know to act on User's pick, at most 400 characters."}
        },
        "required": ["suggest", "topic", "questions", "found", "handoff"],
        "additionalProperties": false
    })
}

fn words_schema() -> Value {
    json!({"type": "object", "properties": {"words": {"type": "string", "description": "One to four plain words."}}, "required": ["words"], "additionalProperties": false})
}

/// The tools of a run: two read-only searches and the answer.
pub fn tools() -> Vec<Tool> {
    vec![
        Tool {
            name: SEARCH_FILES,
            description: "Search User's Mac for files, by name and contents, under his home folder (Spotlight). Returns up to 12 paths. Library caches are left out.",
            schema: words_schema(),
        },
        Tool {
            name: SEARCH_TIMELINE,
            description: "Search Heimdall's archive of what was said, User's and the people he talks with. Returns up to 8 matching lines with their times.",
            schema: words_schema(),
        },
        Tool {
            name: SUBMIT,
            description: "Give the one answer, last. suggest false means show nothing.",
            schema: submit_schema(),
        },
    ]
}

/// The run on the background model: the system prompt and the tools' schemas (the call's hashed prefix), the prompt.
pub fn call(model: &str, prompt: Prompt) -> background_call::Call<'static> {
    let schema = json!({"tools": tools().iter().map(|t| t.name).collect::<Vec<_>>(), "submit": submit_schema()});
    background_call::Call { job: JOB, model: model.to_owned(), system: SYSTEM, schema, effort: EFFORT, prompt, history: Vec::new(), session: None }
}

/// The user message: the people Heimdall knows by name (stable, cached), then the talk and the instruction (volatile).
pub fn prompt(w: &Window, now: u64, known: &[String]) -> Prompt {
    let mut p = Prompt::default();
    if !known.is_empty() {
        p.stable(format!("People Heimdall knows by name: {}.\n\n", known.join(", ")));
    }
    p.push(format!(
        "Now: {}. The talk below is the last ten minutes, oldest first. User is the one labelled User.\n<transcript>\n{}\n</transcript>\n\nDecide whether one card is worth showing User now. Then call {SUBMIT} once.",
        agent::clock::local_hm(now),
        w.transcript()
    ));
    p
}

/// Collapses whitespace and cuts to `max` characters.
fn clean(s: &str, max: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.chars().take(max).collect()
}

/// A person as the words of a card name one: a pronoun for a crowd, or a capitalised name ("Priya", not "CAD").
fn is_person(w: &str) -> bool {
    let lower = w.to_lowercase();
    ["him", "her", "them", "someone", "everyone", "people", "friends"].contains(&lower.as_str())
        || (w.chars().next().is_some_and(char::is_uppercase) && w.chars().skip(1).any(char::is_lowercase))
}

/// Whether a card's wording proposes contacting someone: a reaching verb with a person right after it, or a send
/// verb with a person within four words. A safety net; the prompt says it too.
pub fn proposes_contact(text: &str) -> bool {
    let words: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    words.iter().enumerate().any(|(i, w)| {
        let lower = w.to_lowercase();
        (REACH.contains(&lower.as_str()) && words.get(i + 1).is_some_and(|n| is_person(n)))
            || (SEND.contains(&lower.as_str()) && words[i + 1..].iter().take(4).any(|n| is_person(n)))
            || (lower == "mail" && i > 0 && words[i - 1].eq_ignore_ascii_case("e") && words.get(i + 1).is_some_and(|n| is_person(n)))
    })
}

/// Whether a card's wording asks for an action outside a look-up (deleting, buying, installing, publishing).
pub fn acts_outward(text: &str) -> bool {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| OUTWARD.contains(&w))
}

/// Typed feedback: "bad suggestion", "irrelevant" or "not relevant", said alone or with "that was a" / "thanks" around it.
/// Anything more (an instruction after it, a question) is an ordinary message and is not feedback.
pub fn feedback_phrase(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    let mut words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    if words.len() > 6 {
        return None;
    }
    while words.first().is_some_and(|w| ["that", "this", "it", "was", "is", "s", "a", "an", "the", "really", "so", "just"].contains(w)) {
        words.remove(0);
    }
    while words.last().is_some_and(|w| ["thanks", "thank", "you", "sorry", "please"].contains(w)) {
        words.pop();
    }
    match words.join(" ").as_str() {
        "bad suggestion" => Some("bad suggestion"),
        "irrelevant" => Some("irrelevant"),
        "not relevant" => Some("irrelevant"),
        _ => None,
    }
}

/// Checks the answer in code. `seen` are the paths the searches of this run returned; a `found` entry must be one of them.
pub fn verify(answer: &str, seen: &[String]) -> Result<Verdict> {
    let v: Value = serde_json::from_str(answer).context("the answer is not JSON")?;
    let topic = clean(v["topic"].as_str().unwrap_or(""), 60);
    if v["suggest"] != true {
        return Ok(Verdict::Silent(format!("quiet: {topic}")));
    }
    if topic.is_empty() {
        return Ok(Verdict::Silent("no topic".into()));
    }
    let handoff = clean(v["handoff"].as_str().unwrap_or(""), 400);
    // The topic and the handoff reach the agent too: a contact in either is as much a proposal as one on the card.
    if proposes_contact(&topic) || proposes_contact(&handoff) {
        return Ok(Verdict::Silent("proposes contacting someone".into()));
    }
    let qs = v["questions"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if qs.is_empty() || qs.len() > 3 {
        return Ok(Verdict::Silent(format!("{} questions, not one to three", qs.len())));
    }
    let mut questions = vec![];
    for (i, q) in qs.iter().enumerate() {
        let text = clean(q["text"].as_str().unwrap_or(""), 160);
        let mut options: Vec<String> = q["options"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|o| clean(o, 40)).collect();
        let distinct: BTreeSet<&String> = options.iter().collect();
        if text.is_empty() || !(2..=4).contains(&options.len()) || distinct.len() != options.len() || options.iter().any(String::is_empty) {
            return Ok(Verdict::Silent(format!("question {} is malformed", i + 1)));
        }
        if proposes_contact(&text) || options.iter().any(|o| proposes_contact(o)) {
            return Ok(Verdict::Silent("proposes contacting someone".into()));
        }
        if acts_outward(&text) || options.iter().any(|o| acts_outward(o)) {
            return Ok(Verdict::Silent("asks for an action outside a look-up".into()));
        }
        // Every question carries "Not now": it is the dismissal the pacing counts on. Added in place of the last option
        // when the model left it out of a full list.
        if !options.iter().any(|o| o.eq_ignore_ascii_case("Not now")) {
            if options.len() < 4 {
                options.push("Not now".into());
            } else {
                *options.last_mut().unwrap() = "Not now".into();
            }
        }
        questions.push(Question { id: format!("q{}", i + 1), text, options, multi_select: q["multi_select"] == true });
    }
    let found = v["found"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| Some(Found { path: f["path"].as_str()?.to_owned(), note: clean(f["note"].as_str().unwrap_or(""), 160) }))
        .filter(|f| seen.contains(&f.path))
        .take(3)
        .collect();
    Ok(Verdict::Card(Suggestion { topic, questions, found, handoff }))
}

/// The island card: the topic as its line and the questions as the model wrote them, Other allowed on each.
pub fn card(id: &str, s: &Suggestion, expires_ms: u64) -> ui::agent_notice::AgentNotice {
    use ui::agent_notice::{AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};
    AgentNotice {
        id: id.into(),
        text: s.topic.clone(),
        kind: NoticeKind::Info,
        action: Some(NoticeAction::Ask {
            request_id: id.into(),
            questions: s
                .questions
                .iter()
                .map(|q| AskQuestion {
                    id: q.id.clone(),
                    header: None,
                    text: q.text.clone(),
                    multi_select: q.multi_select,
                    allow_other: true, field: None,
                    options: q.options.iter().map(|o| AskOption::label(o)).collect(),
                })
                .collect(),
            expires_ms,
            countdown: false,
        }),
        outcome: None,
        expires_ms: None,
        detail: None,
    }
}

/// The card as one log line: each question with its options, in order.
pub fn card_line(s: &Suggestion) -> String {
    s.questions.iter().map(|q| format!("{} [{}]", q.text, q.options.join(" | "))).collect::<Vec<_>>().join("; ")
}

/// The user's answers as one line: each question's picks (and what he typed).
pub fn choice(answers: &[ui::agent_notice::QuestionAnswer]) -> String {
    answered(answers)
        .map(|a| {
            let typed = a.other.as_deref().map(|o| format!(" (typed: {o})")).unwrap_or_default();
            format!("{}{typed}", a.labels.join(", "))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The questions the user answered: Send goes with some left (bug note 2026-11-02T16-27-21), and a question left has no
/// labels and no words.
fn answered(answers: &[ui::agent_notice::QuestionAnswer]) -> impl Iterator<Item = &ui::agent_notice::QuestionAnswer> {
    answers.iter().filter(|a| !a.labels.is_empty() || a.other.is_some())
}

/// Whether the user's answers only dismiss the card ("Not now" for every question he answered, nothing typed).
pub fn dismissed(answers: &[ui::agent_notice::QuestionAnswer]) -> bool {
    answered(answers).next().is_some()
        && answered(answers).all(|a| a.other.is_none() && !a.labels.is_empty() && a.labels.iter().all(|l| DISMISS.iter().any(|d| l.eq_ignore_ascii_case(d))))
}

/// The structured notice that wakes the main agent when the user picks an option: the suggestion, the talk before it, what
/// the search found, and his answer, in the words the agent needs to act at once.
pub fn handoff(s: &Suggestion, answers: &[ui::agent_notice::QuestionAnswer], shown_ms: u64, during_call: bool, transcript: &str) -> String {
    let mut out = format!(
        "[Heimdall suggestion, picked by User] This was a suggestion from the island, made at {} from the talk below. User picked this; act on it now with the context here.\nTopic: {}\n",
        agent::clock::local_hm(shown_ms),
        s.topic
    );
    for a in answered(answers) {
        let text = s.questions.iter().find(|q| q.id == a.question_id).map_or("", |q| q.text.as_str());
        let picked = if a.labels.is_empty() { String::new() } else { a.labels.join(", ") };
        let typed = a.other.as_deref().map(|o| format!(" (typed: {o})")).unwrap_or_default();
        out.push_str(&format!("Question \"{text}\": User picked {picked}{typed}.\n"));
    }
    if !s.found.is_empty() {
        out.push_str("Found by Heimdall's search:\n");
        for f in &s.found {
            out.push_str(&format!("- {}: {}\n", f.path, f.note));
        }
    }
    if !s.handoff.is_empty() {
        out.push_str(&format!("Notes: {}\n", s.handoff));
    }
    if during_call {
        out.push_str("User is on a call: answer on screen only, and do not speak.\n");
    }
    out.push_str(&format!("Talk before the card (oldest first):\n{transcript}"));
    out
}

/// The feedback phrase the user typed, if it is one: "bad suggestion" or "irrelevant" (or "not relevant") in a short line.
pub fn feedback(text: &str) -> Option<&'static str> {
    feedback_phrase(text)
}

/// Where the suggester reads and writes: the user's home (the files), Heimdall's data folder (the log, never searched) and
/// the archive's index (read-only, for the timeline search).
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub data: PathBuf,
    pub archive: Option<PathBuf>,
}

/// Plain words for Spotlight and the archive: at most four, letters and digits only, each at most 24 characters.
pub fn query(words: &str) -> Option<String> {
    let w: Vec<String> = words
        .split_whitespace()
        .map(|w| w.chars().filter(|c| c.is_alphanumeric()).take(24).collect::<String>())
        .filter(|w| !w.is_empty())
        .take(4)
        .collect();
    (!w.is_empty()).then(|| w.join(" "))
}

/// Paths a file search may return: under home, not Library caches, the Trash, version control, build folders or
/// Heimdall's own data.
pub fn keep_path(path: &str, home: &Path, data: &Path) -> bool {
    let p = Path::new(path);
    p.starts_with(home)
        && !p.starts_with(data)
        && !path.contains("/Library/Caches/")
        && !path.contains("/.Trash/")
        && !path.contains("/.git/")
        && !path.contains("/node_modules/")
        && !path.contains("/target/")
}

/// Spotlight over the user's home, read-only: at most `MAX_FILES` paths `keep_path` allows.
pub fn search_files(words: &str, home: &Path, data: &Path) -> Result<Vec<String>> {
    // Without a home folder the filter would allow every path: no search at all.
    if home.as_os_str().is_empty() {
        bail!("no home folder to search");
    }
    let Some(q) = query(words) else { bail!("no words to search for") };
    let out = std::process::Command::new("/usr/bin/mdfind").arg("-onlyin").arg(home).arg(&q).output().context("mdfind")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|p| keep_path(p, home, data))
        .take(MAX_FILES)
        .map(str::to_owned)
        .collect())
}

/// The archive's text lines matching `words`, read-only: `[HH:MM source] words` per hit, at most `MAX_HITS`.
pub fn search_timeline(db: &Path, words: &str) -> Result<String> {
    let Some(q) = query(words) else { bail!("no words to search for") };
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    let hits = heimdall_archive::timeline::search(
        &conn,
        &heimdall_archive::timeline::Search { text: Some(q), kinds: vec!["text".into()], limit: MAX_HITS, ..Default::default() },
    )?;
    if hits.is_empty() {
        return Ok("No matching lines.".into());
    }
    Ok(hits
        .iter()
        .map(|c| format!("[{} {}] {}", agent::clock::local_hm(c.start_wall_ns / 1_000_000), c.source, clean(&c.text, 300)))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// One run on `e` (`background_model::endpoint`): the model's answer, its searches run here, the answer checked by
/// `verify`. Blocking, like the other background jobs: call it from a thread outside the async runtime.
pub fn ask(e: &Endpoint, prompt: Prompt, paths: &Paths) -> Result<Verdict> {
    let model = e.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
    let c = call(&model, prompt);
    let seen: RefCell<Vec<String>> = RefCell::new(vec![]);
    let searches = std::cell::Cell::new(0usize);
    let mut run = |name: &str, input: &Value| -> String {
        let words = input["words"].as_str().unwrap_or("");
        if matches!(name, SEARCH_FILES | SEARCH_TIMELINE) {
            if searches.get() >= MAX_SEARCHES {
                return format!("Search limit reached: {MAX_SEARCHES} searches a run. Give your answer now with {SUBMIT}.");
            }
            searches.set(searches.get() + 1);
        }
        match name {
            SEARCH_FILES => match search_files(words, &paths.home, &paths.data) {
                Ok(found) if found.is_empty() => "No files found.".into(),
                Ok(found) => {
                    let text = found.join("\n");
                    seen.borrow_mut().extend(found);
                    text
                }
                Err(err) => format!("Search failed: {err:#}"),
            },
            SEARCH_TIMELINE => match &paths.archive {
                Some(db) => search_timeline(db, words).unwrap_or_else(|err| format!("Search failed: {err:#}")),
                None => "The archive is not available.".into(),
            },
            other => format!("Unknown tool {other}."),
        }
    };
    let answer = background_call::ask_tools_blocking(e, &c, &tools(), SUBMIT, MAX_ROUNDS, &mut run)?;
    verify(&answer.text, &seen.into_inner())
}

/// The log file under Heimdall's data folder.
pub fn log_path(data: &Path) -> PathBuf {
    data.join("suggestions").join("log.jsonl")
}

/// Appends one event line to the log (`{"id", "event", ...}`), synced.
pub fn append(path: &Path, event: &Value) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{event}")?;
    f.sync_data()?;
    Ok(())
}

/// The decider's "suggest now" labels from the log (v4's training input), one row per card. `suggest_now` is true for a
/// pick, false for a dismissal or typed "bad suggestion"/"irrelevant" (the decider learns what not to offer), and null
/// for an ignored card or one still open: no signal. Silent decisions are not cards and are not exported.
pub fn export(log: &str) -> Vec<Value> {
    let mut cards: Vec<(String, Value)> = vec![];
    for line in log.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(ev) = serde_json::from_str::<Value>(line) else { continue };
        let Some(id) = ev["id"].as_str().map(str::to_owned) else { continue };
        match ev["event"].as_str() {
            Some("shown") => cards.push((id.clone(), json!({"id": id, "at": ev["at"], "topic": ev["topic"], "card": ev["card"], "transcript": ev["transcript"], "suggest_now": Value::Null, "outcome": "shown"}))),
            Some(kind) => {
                let Some((_, row)) = cards.iter_mut().find(|(i, _)| *i == id) else { continue };
                match kind {
                    "picked" => {
                        row["suggest_now"] = json!(true);
                        row["outcome"] = json!("picked");
                        row["choice"] = ev["choice"].clone();
                    }
                    "dismissed" => {
                        row["suggest_now"] = json!(false);
                        row["outcome"] = json!("dismissed");
                    }
                    "ignored" => row["outcome"] = json!("ignored"),
                    "feedback" => {
                        row["suggest_now"] = json!(false);
                        row["outcome"] = json!("feedback");
                        row["feedback"] = ev["kind"].clone();
                    }
                    _ => {}
                }
            }
            None => {}
        }
    }
    cards.into_iter().map(|(_, row)| row).collect()
}

#[cfg(test)]
#[path = "suggester_tests.rs"]
mod tests;
