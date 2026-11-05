//! The gallery's cards: one named scenario per island state and transition (DESIGN.md §3), played by the tests under
//! every invariant and rendered frame by frame by `heimdall-ui-demo --gallery` (lab/animation-gallery) through the
//! same `Island` and the same paint call the live window uses.

use super::sim::{Scenario, Step};
use super::{Display, Event, Switch, Target};
use crate::agent_notice::{AgentNotice, AskOption, AskQuestion, Detail, NoticeAction, NoticeKind, QuestionAnswer};
use crate::island::{Activity, CommandCue, CommandKind, HealthControl, HealthCue, Part};
use crate::model::Permission;
use crate::reader::{FastRead, FastReadUpdate};

/// One gallery card.
#[derive(Clone, Debug)]
pub struct Card {
    pub name: &'static str,
    pub title: &'static str,
    pub group: &'static str,
    pub what: &'static str,
    pub scenario: Scenario,
    /// Seconds to render.
    pub end: f64,
    /// Labels under the scrubber: (time, label).
    pub marks: Vec<(f64, &'static str)>,
    /// Rendered as the whole screen, scaled down (the drawing light round the edge, pointing marks).
    pub full: bool,
}

fn ev(t: f64, e: Event) -> (f64, Step) {
    (t, Step::Ev(e))
}

fn line(id: &str, text: &str, kind: NoticeKind) -> Event {
    Event::Notice { notice: AgentNotice::line(id, text, kind), body: None }
}

/// The two replies of 22 Oct 15:12 (bug note 15:13), shortened.
const TWO_REPLIES: [&str; 2] = [
    "I found it: it's the Mistral voice agent demo, with two people talking to Charlotte. It runs three stages streaming at once, and a worker is researching how it works.",
    "Right. I can fetch a page through the shell, but I have no proper search tool. I'd add one tonight; it needs a search key such as Brave or Exa. Do you have one?",
];

/// One question of a card: its header (the step chip), whether several options may be picked, and its options with
/// their descriptions.
fn ask_q(id: &str, header: Option<&str>, multi: bool, options: &[(&str, &str)]) -> AskQuestion {
    AskQuestion {
        id: id.into(),
        header: header.map(str::to_owned),
        text: format!("{} question", header.unwrap_or("Which")),
        multi_select: multi,
        allow_other: true, field: None,
        options: options.iter().map(|(l, d)| AskOption { label: (*l).into(), description: (!d.is_empty()).then(|| (*d).into()), grant: vec![] }).collect(),
    }
}

/// A card of one or more questions (`ask` with `questions`).
fn ask_card(id: &str, questions: Vec<AskQuestion>, expires_ms: u64) -> Event {
    let mut n = AgentNotice::line(id, "Questions", NoticeKind::Info);
    n.action = Some(NoticeAction::Ask { request_id: id.into(), questions, expires_ms, countdown: false });
    Event::Notice { notice: n, body: None }
}

fn picked(question: &str, labels: &[&str]) -> QuestionAnswer {
    QuestionAnswer { question_id: question.into(), labels: labels.iter().map(|l| (*l).into()).collect(), other: None }
}

fn ask(id: &str, text: &str, options: &[&str], expires_ms: u64) -> Event {
    let mut n = AgentNotice::line(id, text, NoticeKind::Info);
    n.action = Some(NoticeAction::single(id, text, options, expires_ms));
    Event::Notice { notice: n, body: None }
}

/// A voice card as `pipeline/voice_names.rs` sends it: its guessed names as chips, the name field,
/// then Not important last. The field never asks for the keyboard: every voice card comes on its own.
fn voice_card(id: &str, text: &str, options: &[&str], expires_ms: u64) -> Event {
    let mut n = AgentNotice::line(id, text, NoticeKind::Info);
    let mut q = AskQuestion::plain("q1", text, options);
    q.allow_other = true;
    q.field = Some(crate::agent_notice::OtherField { placeholder: "Type the name".into(), focus_until_ms: 0, after: 1 });
    n.action = Some(NoticeAction::Ask { request_id: id.into(), questions: vec![q], expires_ms, countdown: false });
    Event::Notice { notice: n, body: None }
}

fn speak(t: f64, text: &str, seconds: f64) -> Vec<(f64, Step)> {
    let mut v = vec![ev(t, Event::Speaking(true)), ev(t + 0.12, Event::SpokenText(Some(text.into())))];
    let n = (seconds / 0.033) as usize;
    for k in 1..n {
        let x = k as f32 * 0.37;
        v.push(ev(t + k as f64 * 0.033, Event::SpeechLevel((0.45 + 0.3 * x.sin() * (x * 0.31).cos()).clamp(0.0, 1.0))));
    }
    v.push(ev(t + seconds, Event::Speaking(false)));
    v
}

/// A reply spoken sentence by sentence: each sentence shows as it starts playing and stays, the next added below it
/// (`SpokenText` per sentence, as playback sends them), the levels throughout, the audio's end after the last.
fn speak_reply(t: f64, sentences: &[(&str, f64)]) -> Vec<(f64, Step)> {
    let mut v = vec![ev(t, Event::Speaking(true))];
    let mut at = t + 0.12;
    for (text, seconds) in sentences {
        v.push(ev(at, Event::SpokenText(Some(text.to_string()))));
        at += seconds;
    }
    let end = at;
    let n = ((end - t) / 0.033) as usize;
    for k in 1..n {
        let x = k as f32 * 0.37;
        v.push(ev(t + k as f64 * 0.033, Event::SpeechLevel((0.45 + 0.3 * x.sin() * (x * 0.31).cos()).clamp(0.0, 1.0))));
    }
    v.push(ev(end, Event::Speaking(false)));
    v
}

/// The reply the speaking cards say.
pub const REPLY: [(&str, f64); 3] = [
    ("The build is green.", 1.1),
    ("Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.", 3.0),
    ("Say the word and I'll install it.", 1.4),
];

/// A long reply (more lines than the speaking row holds).
pub const LONG_REPLY: [(&str, f64); 8] = [
    ("Three things changed since this morning.", 1.6),
    ("The island now keeps every sentence I say until I stop speaking.", 2.2),
    ("Lines that used to replace each other are added below instead.", 2.0),
    ("Nothing celebrates after I finish: the island simply retracts.", 2.0),
    ("Pull the notch down to read the whole reply again.", 1.8),
    ("It scrolls when it is longer than half the screen.", 1.6),
    ("The animations start their bounce as the shape lands.", 1.8),
    ("That is everything for today.", 1.2),
];

/// The table note the mockups show.
pub fn table_note() -> AgentNotice {
    let mut n = AgentNotice::line("note-tests", "Test times", NoticeKind::Info);
    n.detail = Some(Detail::Table {
        headers: vec!["Target".into(), "Tests".into(), "Time".into()],
        rows: vec![
            vec!["heimdall-ui".into(), "214".into(), "41 s".into()],
            vec!["heimdall-listen".into(), "96".into(), "1 m 58 s".into()],
            vec!["heimdall-agent".into(), "131".into(), "2 m 04 s".into()],
        ],
    });
    n
}

/// The last turn of `chat_day`: its reply as written.
pub const CHAT_REPLY: &str = "**About 61 GB in use, and no swap.**\n\nThe decider retrain is the biggest at 38 GB; Heimdall itself holds 14 GB.";

/// The table that turn attached.
pub fn memory_note() -> AgentNotice {
    let mut n = AgentNotice::line("notice-memory", "Memory by job", NoticeKind::Info);
    let row = |a: &str, b: &str| vec![a.to_string(), b.to_string()];
    n.detail = Some(Detail::Table {
        headers: vec!["Job".into(), "Memory".into()],
        rows: vec![row("Decider v3 retrain", "38.2 GB"), row("Heimdall", "14.1 GB"), row("Arsenal (Qwen-Image)", "6.4 GB"), row("Everything else", "2.6 GB")],
    });
    n
}

/// An evening's conversation as the app sends it (`chat_history`): ending with the last turn's own reply and table.
pub fn chat_day() -> Vec<crate::chat::ChatEntry> {
    use crate::chat::{ActionKind, ChatEntry, ChatKind, Heard, NoteShape};
    let owner = |s: &str| ChatKind::Owner(s.into());
    let him = |s: &str, heard| ChatKind::Heimdall { text: s.into(), heard };
    let act = |kind, s: &str, a: Option<&str>| ChatKind::Action { kind, text: s.into(), answer: a.map(Into::into) };
    let rows = vec![
        owner("Heimdall, are the English acks ready?"),
        him("Yes. Thirty-two plain lines and twenty scenario versions, all English. The player is open in Ego Lite.", Heard::Whole),
        ChatKind::Note { id: "notice-acks".into(), title: "English acks".into(), shape: NoteShape::Table },
        owner("Cut the Polish ones and commit it."),
        act(ActionKind::Started, "Ack lines: crisper set", None),
        him("On it. Every Polish line goes from the set, the decider scenarios and the player.", Heard::Cut),
        act(ActionKind::Finished, "Ack lines: crisper set", None),
        act(ActionKind::Question, "Vella 2.1.1 is signed. Publish it?", Some("Hold draft")),
        him("Holding the draft. Nothing goes out until you say so.", Heard::Never),
        owner("How much memory are the jobs using right now?"),
        ChatKind::Note { id: "notice-memory".into(), title: "Memory by job".into(), shape: NoteShape::Table },
        him(CHAT_REPLY, Heard::Whole),
    ];
    rows.into_iter().enumerate().map(|(k, kind)| ChatEntry { at_ms: 1_792_780_000_000 + k as u64 * 60_000, kind }).collect()
}

/// The user's 23:11 table (bug note 2026-10-20T23-12-49): four columns, the role the longest.
pub fn jobs_note() -> AgentNotice {
    let mut n = AgentNotice::line("notice-jobs", "Local AI jobs Warsaw", NoticeKind::Info);
    let row = |a: &str, b: &str, c: &str, d: &str| vec![a.to_string(), b.to_string(), c.to_string(), d.to_string()];
    n.detail = Some(Detail::Table {
        headers: vec!["Company".into(), "Role".into(), "Pay".into(), "Where".into()],
        rows: vec![
            row("Agentica", "AI Engineer (Agents)", "12–16k PLN", "Hybrid, Warsaw"),
            row("BlockWise", "Python Dev (LLM / AI Infra)", "9–12k PLN", "On site, Warsaw"),
            row("in4ge", "ML Engineer, Generative AI", "15–20k PLN", "Remote / Warsaw"),
        ],
    });
    n
}

/// A table wider than the pull-down's widest: a long last column.
pub fn wide_note() -> AgentNotice {
    let mut n = AgentNotice::line("notice-wide", "Matches with notes", NoticeKind::Info);
    let row = |a: &str, b: &str, c: &str, d: &str, e: &str| [a, b, c, d, e].iter().map(|s| s.to_string()).collect::<Vec<_>>();
    n.detail = Some(Detail::Table {
        headers: vec!["Company".into(), "Role".into(), "Pay".into(), "Where".into(), "What they ask".into()],
        rows: vec![
            row("Agentica", "AI Engineer (Agents)", "12–16k PLN", "Hybrid, Warsaw", "Two years of Python, agent frameworks, evaluation harnesses and a portfolio"),
            row("BlockWise", "Python Dev (LLM / AI Infra)", "9–12k PLN", "On site, Warsaw", "FastAPI, vector stores, Kubernetes basics"),
            row("in4ge", "ML Engineer, Generative AI", "15–20k PLN", "Remote / Warsaw", "Fine-tuning, inference serving, English C1"),
        ],
    });
    n
}

/// Forty rows: taller than half the screen.
pub fn long_note() -> AgentNotice {
    let mut n = AgentNotice::line("notice-long", "Test times", NoticeKind::Info);
    n.detail = Some(Detail::Table {
        headers: vec!["Target".into(), "Tests".into(), "Time".into()],
        rows: (1..=40).map(|k| vec![format!("crate-{k:02}"), format!("{}", 10 + 7 * k), format!("{} s", 3 * k)]).collect(),
    });
    n
}

pub fn log_note() -> AgentNotice {
    let mut n = AgentNotice::line("note-log", "Build failed", NoticeKind::Error);
    n.detail = Some(Detail::Text(
        "error[E0308]: mismatched types\n  --> crates/ui/src/mac/island.rs:812:17\n   |\n812|     let r = corner(h);\n   |             ^^^^^^^^^ expected f64".into(),
    ));
    n
}

const PAGE_REPLY: &str = "Yes, the lip is fixed and shows under the notch now.\n\nWhat changed tonight:\n- The lip appears only when the pointer reaches the notch, so nothing draws at rest.\n- A click on it opens the last turn.\n- Tab strips keep their clicks.\n\nI checked it with offscreen renders at real size.";
const PAGE_LINE: &str = "The build is green. Two tests were slow.\n\nWhat changed:\n- the island's text layout\n- the speakers store";

const READ_TEXT: &str = "The build failed because tests timed out. Three things matter. First, the limit is two minutes, and the tests need longer. Second, the slow part is loading the embedding model, which happens one target at a time.";

fn card(name: &'static str, title: &'static str, group: &'static str, what: &'static str, steps: Vec<(f64, Step)>, end: f64, marks: Vec<(f64, &'static str)>) -> Card {
    Card { name, title, group, what, scenario: Scenario::new(name, what, steps), end, marks, full: false }
}

/// Every card, in the gallery's order.
pub fn all() -> Vec<Card> {
    let mut v = vec![];
    // Agent
    v.push(card("sent_to_agent", "Sent to agent", "Agent", "User's words went to the agent: the lip with the arrow, then rest.", vec![ev(0.1, Event::Sent)], 2.0, vec![(0.1, "Sent")]));
    v.push(card(
        "agent_thinking",
        "Thinking",
        "Agent",
        "The arrow becomes the thinking dots; the lip holds while the agent thinks.",
        vec![ev(0.1, Event::Sent), ev(0.3, Event::Activity(Some(Activity::Thinking))), ev(3.2, Event::Activity(None))],
        4.0,
        vec![(0.1, "Sent"), (0.3, "Thinking"), (3.2, "Idle")],
    ));
    v.push(card(
        "agent_working",
        "Working",
        "Agent",
        "The dots glide down and the words pop up below them; only the words change; then the lip again.",
        vec![
            ev(0.1, Event::Activity(Some(Activity::Thinking))),
            ev(0.8, line("w1", "On it", NoticeKind::Working)),
            ev(2.4, line("w2", "Checking build", NoticeKind::Working)),
            ev(7.8, Event::Activity(None)),
        ],
        8.6,
        vec![(0.1, "Thinking"), (0.8, "On it"), (2.4, "Checking build"), (7.8, "Idle")],
    ));
    v.push(card(
        "agent_compacting",
        "Compacting",
        "Agent",
        "Compacting reads for 2.5 s, then rests as a breathing lip until it ends.",
        vec![ev(0.1, Event::Activity(Some(Activity::Compacting))), ev(4.6, Event::Activity(None))],
        5.4,
        vec![(0.1, "Compacting"), (4.6, "Done")],
    ));
    let mut turn = vec![ev(0.1, Event::Sent), ev(0.35, Event::Activity(Some(Activity::Thinking)))];
    turn.extend(speak(1.4, "The build is green.", 1.4));
    turn.push(ev(2.0, Event::Activity(None)));
    v.push(card("agent_turn", "A whole turn", "Agent", "Sent, thinking, speaking with words, then it simply retracts: one shape, vertical morphs, no success mark at the end.", turn, 4.0, vec![(0.1, "Sent"), (0.35, "Thinking"), (1.4, "Speaking"), (2.8, "Voice ends")]));
    v.push(card(
        "done_silent",
        "Done, silent turn",
        "Agent",
        "A turn that spoke nothing ends with what got done.",
        vec![ev(0.1, Event::Activity(Some(Activity::Thinking))), ev(1.0, Event::Activity(None)), ev(1.0, line("ok", "Build green", NoticeKind::Success))],
        5.6,
        vec![(1.0, "Build green")],
    ));
    // Voice
    v.push(card("speaking_words", "Speaking with words", "Voice", "Stop and waveform in the ears, the sentence below; the bars flatten the frame the audio ends.", speak(0.1, "The build is green.", 2.0), 3.0, vec![(0.1, "Audio starts"), (2.1, "Audio ends")]));
    // Bug notes 2026-11-05T00-10-37 / 02-32-17: Stop in the left ear while the voice plays; a press cuts it at once.
    let mut stop: Vec<(f64, Step)> = speak(0.1, "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.", 3.0).into_iter().filter(|(t, _)| *t < 1.6).collect();
    stop.push((1.6, Step::Ev(Event::Click(Target::StopSpeech))));
    stop.push(ev(1.65, Event::SpeechCut));
    stop.push(ev(1.7, Event::Speaking(false)));
    v.push(card("speaking_stop", "Stop while speaking", "Voice", "Stop in the left ear while the voice plays; a press cuts the voice and the words at once.", stop, 3.0, vec![(0.1, "Audio starts"), (1.6, "Stop")]));
    v.push(card(
        "speaking_long",
        "Speaking, a long sentence",
        "Voice",
        "The row grows to the sentence's lines; the ears stay.",
        speak(0.1, "Dave pushed the audio fix an hour ago, and I kept the old build as the rollback.", 3.0),
        4.0,
        vec![(0.1, "Audio starts"), (3.1, "Audio ends")],
    ));
    v.push(card(
        "speaking_reply",
        "Speaking a reply, sentence by sentence",
        "Voice",
        "Each sentence shows as it is spoken and stays; the next is added below it; when the voice ends the island retracts.",
        speak_reply(0.1, &REPLY),
        6.6,
        vec![(0.22, "Sentence 1"), (1.32, "Sentence 2"), (4.32, "Sentence 3"), (5.72, "Voice ends")],
    ));
    v.push(card(
        "speaking_long_reply",
        "Speaking a long reply",
        "Voice",
        "More than six lines: the oldest sentence leaves at the top and the rest glide up; the whole reply waits in the pull-down.",
        speak_reply(0.1, &LONG_REPLY),
        15.2,
        vec![(0.22, "1"), (5.92, "4"), (9.72, "6"), (14.52, "Voice ends")],
    ));
    let mut with_line = speak(0.1, "The build is green.", 4.2);
    with_line.push(ev(1.2, line("tp", "Tests passed", NoticeKind::Success)));
    v.push(card("speaking_with_line", "A line while speaking", "Voice", "A line arrives while the voice plays: it rides above the words, which stay.", with_line, 5.4, vec![(1.2, "Tests passed")]));
    let mut pull = speak_reply(0.1, &REPLY);
    pull.push(ev(7.0, Event::Click(Target::Token(super::Token::Reply))));
    pull.push(ev(9.6, Event::Hover(Some(Target::ReplyBand))));
    pull.push(ev(10.0, Event::Click(Target::ReplyBand)));
    v.push(card(
        "reply_pull_down",
        "Pull down the last reply",
        "Voice",
        "After the voice has retracted, pulling the notch down (a click, a drag or a swipe) shows the last reply whole; its lip pulls it up.",
        pull,
        10.8,
        vec![(5.72, "Voice ends"), (7.0, "Pull down"), (10.0, "Pull up")],
    ));
    let mut pull_long = speak_reply(0.1, &LONG_REPLY);
    pull_long.push(ev(15.0, Event::Click(Target::Token(super::Token::Reply))));
    for k in 0..6 {
        pull_long.push(ev(16.2 + k as f64 * 0.05, Event::Scroll(10.0)));
    }
    pull_long.push(ev(17.6, Event::Click(Target::ReplyBand)));
    v.push(card(
        "reply_pull_down_long",
        "Pull down a long reply",
        "Voice",
        "A reply longer than the row: pulled down it shows every sentence, scrolling inside the shape when it would pass half the screen.",
        pull_long,
        18.4,
        vec![(15.0, "Pull down"), (16.2, "Scroll"), (17.6, "Pull up")],
    ));
    // Night pass 1022: a reply's paragraphs and lists (bug note 2026-10-21T19-21-01), and a send pulling up the
    // pulled-down turn (bug note 2026-10-21T18-50-16).
    let mut page = vec![ev(0.1, Event::LastReply(PAGE_REPLY.into()))];
    page.push(ev(0.6, Event::Click(Target::Token(super::Token::Reply))));
    page.push(ev(3.2, Event::Click(Target::ReplyBand)));
    v.push(card(
        "reply_pull_down_page",
        "Pull down a reply with paragraphs",
        "Voice",
        "A reply with paragraphs and a list keeps them: the answer bold, a gap between paragraphs, list lines hung under their bullets.",
        page,
        4.0,
        vec![(0.6, "Pull down"), (3.2, "Pull up")],
    ));
    v.push(card(
        "peek_lip",
        "The lip under the notch",
        "Voice",
        "With a last turn to open, the pointer reaching the notch shows a short lip just below it; leaving hides it; a click opens the last turn.",
        vec![
            ev(0.1, Event::LastReply(PAGE_REPLY.into())),
            ev(0.6, Event::Hover(Some(Target::Token(super::Token::Reply)))),
            ev(1.8, Event::Hover(None)),
            ev(2.6, Event::Hover(Some(Target::Token(super::Token::Reply)))),
            ev(3.4, Event::Click(Target::Token(super::Token::Reply))),
        ],
        4.6,
        vec![(0.6, "Pointer at the notch"), (1.8, "Leaves"), (3.4, "Click")],
    ));
    v.push(card(
        "sent_pulls_up_the_turn",
        "Sent while pulled down",
        "Agent",
        "The last turn is pulled down and User speaks: when his words reach the agent, the pull-down pulls up into the send arrow, then the thinking dots.",
        vec![
            ev(0.1, Event::LastReply(PAGE_REPLY.into())),
            ev(0.4, Event::Click(Target::Token(super::Token::Reply))),
            ev(2.0, Event::Sent),
            ev(2.8, Event::Activity(Some(Activity::Thinking))),
        ],
        4.2,
        vec![(0.4, "Pull down"), (2.0, "Sent"), (2.8, "Thinking")],
    ));
    v.push(card(
        "listening_on",
        "Listening on",
        "Voice",
        "Listening turned on: helmet and spinner while the models load, then Listening.",
        vec![ev(0.1, Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::Listening })), ev(1.4, Event::Command(CommandCue::Done { id: Some(1), kind: CommandKind::Listening, label: "Listening".into() }))],
        3.8,
        vec![(0.1, "Loading"), (1.4, "Listening")],
    ));
    // Lines
    for (name, title, text, kind) in [
        ("line_info", "Info", "Dave's fix landed", NoticeKind::Info),
        ("line_success", "Success", "Heimdall installed", NoticeKind::Success),
        ("line_warning", "Warning", "Disk almost full", NoticeKind::Warning),
        ("line_looking", "Looking", "Looking", NoticeKind::Looking),
    ] {
        let read = super::read_seconds(kind, text);
        v.push(card(name, title, "Lines", "Icon over words for its read time (1.8–3.4 s by kind), then it clears itself.", vec![ev(0.1, line(name, text, kind))], read + 1.0, vec![(0.1, title)]));
    }
    v.push(card(
        "line_error",
        "Error",
        "Lines",
        "An error reads (the red x wiggles), then retracts like every line.",
        vec![ev(0.1, line("err", "Build failed", NoticeKind::Error))],
        super::read_seconds(NoticeKind::Error, "Build failed") + 1.0,
        vec![(0.1, "Error")],
    ));
    let sentence = "The build is green. Dave pushed the audio fix an hour ago.";
    v.push(card("line_sentence", "A line to read (voice off)", "Lines", "Up to three lines, no icon, for their read time.", vec![ev(0.1, line("s", sentence, NoticeKind::Info))], super::read_seconds(NoticeKind::Info, sentence) + 1.0, vec![]));
    v.push(card(
        "line_page",
        "A line with paragraphs (voice off)",
        "Lines",
        "A reply shown as lines keeps its paragraphs and list: left-aligned as a page, the answer bold.",
        vec![ev(0.1, line("p", PAGE_LINE, NoticeKind::Info))],
        super::read_seconds(NoticeKind::Info, PAGE_LINE) + 1.0,
        vec![],
    ));
    v.push(card(
        "line_updates",
        "A line updated in place",
        "Lines",
        "The agent updates its own line by id: only the words change.",
        vec![ev(0.1, line("u", "Installing", NoticeKind::Info)), ev(1.6, line("u", "Heimdall installed", NoticeKind::Success))],
        6.4,
        vec![(0.1, "Installing"), (1.6, "Installed")],
    ));
    let flood: Vec<(f64, Step)> = (0..12).map(|k| ev(0.1 + k as f64 * 0.08, line(&format!("f{k}"), &format!("Icon test {}", k + 1), NoticeKind::Info))).collect();
    v.push(card("line_flood", "A flood of lines", "Lines", "Twelve lines in a second: the island shows the newest, never a list, and clears.", flood, 6.4, vec![(0.1, "12 lines")]));
    // Commands
    v.push(card(
        "command_heard_done",
        "Heard, then done",
        "Commands",
        "Eye and hourglass in the ears while the turn runs, then Screenshot.",
        vec![ev(0.1, Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::Screenshot })), ev(1.8, Event::Command(CommandCue::Done { id: Some(1), kind: CommandKind::Screenshot, label: "Screenshot".into() }))],
        4.4,
        vec![(0.1, "Heard"), (1.8, "Done")],
    ));
    v.push(card(
        "command_heard_dropped",
        "Heard, then dropped",
        "Commands",
        "A command that was not one: the ears retract silently.",
        vec![ev(0.1, Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::LookAtThis })), ev(1.6, Event::Command(CommandCue::Dropped { id: 1 }))],
        2.6,
        vec![(0.1, "Heard"), (1.6, "Dropped")],
    ));
    let mut counts = vec![];
    for k in 0..3u64 {
        counts.push(ev(0.1 + k as f64 * 0.7, Event::Command(CommandCue::Done { id: None, kind: CommandKind::Screenshot, label: "Screenshot".into() })));
    }
    v.push(card("command_counts", "Three screenshots", "Commands", "Each screenshot counts on: only the numeral changes.", counts, 4.6, vec![(0.1, "1"), (0.8, "2"), (1.5, "3")]));
    v.push(card(
        "command_failed",
        "Command couldn't run",
        "Commands",
        "A warning and why.",
        vec![ev(0.1, Event::Command(CommandCue::Failed { id: None, kind: CommandKind::Screenshot, reason: "Seeing is off".into() }))],
        3.4,
        vec![(0.1, "Failed")],
    ));
    // Question card
    v.push(card(
        "question_two",
        "Question: Yes / No",
        "Question card",
        "White answer capsules; a click answers and it retracts at once.",
        vec![ev(0.1, ask("q", "Install the new build?", &["Yes", "No"], 600_000)), (2.0, Step::ClickAny(0))],
        3.4,
        vec![(0.1, "Asks"), (2.0, "Yes")],
    ));
    // Quiet speaker naming (23 Oct): the voice namer's card for a voice that keeps coming back (pipeline/voice_names.rs).
    v.push(card(
        "question_voice_name",
        "Question: who is this voice",
        "Question card",
        "A voice that keeps coming back: its best-guess name and the line it rests on; Yes, type the name, or Not important.",
        vec![ev(0.1, voice_card("voice-name:Speaker Wed 21 Oct 20h #2", "Is this Dave? \u{201c}User: Dave, what do you think?\u{201d}", &["Yes", "Not important"], 600_000)), (2.4, Step::ClickAny(0))],
        3.6,
        vec![(0.1, "Asks"), (2.4, "Yes")],
    ));
    // New-voice cards ("voice card v3"): one card per new voice, silent, one at a time. The quote,
    // up to two guessed names as chips only when there is a real guess, the name field, Not important.
    v.push(card(
        "question_new_voice",
        "Question: a new voice, two guesses",
        "Question card",
        "A new voice and a sentence it just said; its two likeliest names one click each, the name field, Not important.",
        vec![ev(0.1, voice_card("voice-new:Speaker Tue 3 Nov 17h #1", "New voice: \u{201c}It was way faster than last week…\u{201d}", &["Mari", "Ellie", "Not important"], 300_000)), (2.4, Step::ClickAny(0))],
        3.6,
        vec![(0.1, "Asks"), (2.4, "Mari")],
    ));
    v.push(card(
        "question_new_voice_one",
        "Question: a new voice, one guess",
        "Question card",
        "One real guess: one chip.",
        vec![ev(0.1, voice_card("voice-new:Speaker Tue 3 Nov 17h #5", "New voice: \u{201c}Let's decide that on Friday then.\u{201d}", &["Kanan", "Not important"], 300_000)), (2.4, Step::ClickAny(0))],
        3.6,
        vec![(0.1, "Asks"), (2.4, "Kanan")],
    ));
    v.push(card(
        "question_new_voice_none",
        "Question: a new voice, no guess",
        "Question card",
        "Nothing names the voice: the quote, the name field and Not important.",
        vec![ev(0.1, voice_card("voice-new:Speaker Tue 3 Nov 17h #2", "New voice: \u{201c}I need to visit the barber.\u{201d}", &["Not important"], 300_000))],
        3.6,
        vec![(0.1, "Asks")],
    ));
    v.push(card(
        "question_new_voice_long",
        "Question: a new voice, longest line and names",
        "Question card",
        "A long sentence in wide letters, cut to the one line; two long names.",
        vec![ev(0.1, voice_card("voice-new:Speaker Tue 3 Nov 17h #6", "New voice: \u{201c}Whoever wins Monday's WWDC…\u{201d}", &["Alexandra Wójcik-Nowak", "Maximilian", "Not important"], 300_000)), (2.4, Step::ClickAny(1))],
        3.6,
        vec![(0.1, "Asks"), (2.4, "Maximilian")],
    ));
    // Bug note 2026-10-28T19-45-55: the card's lip tucks it away unanswered; the notch brings it back.
    v.push(card(
        "question_tuck",
        "Question: tucked by its lip",
        "Question card",
        "The card ends in the pull-down's lip; a click or a drag up tucks it away still pending; the notch brings it back.",
        vec![
            ev(0.1, ask("q", "Push the fix to main?", &["Yes", "No"], 600_000)),
            ev(1.8, Event::Hover(Some(Target::AskTuck("q".into())))),
            ev(2.2, Event::Click(Target::AskTuck("q".into()))),
            ev(3.0, Event::Hover(Some(Target::Token(super::Token::Reply)))),
            ev(3.8, Event::Click(Target::Token(super::Token::Reply))),
        ],
        5.0,
        vec![(0.1, "Asks"), (2.2, "Tuck"), (3.0, "Notch"), (3.8, "Back")],
    ));
    v.push(card("question_four", "Question: four answers", "Question card", "Two to four answers, centred.", vec![ev(0.1, ask("q", "Which build tonight?", &["Main", "Island", "Reader", "None"], 600_000)), (2.4, Step::ClickAny(2))], 3.6, vec![(2.4, "Reader")]));
    // Adaptive card (24 Oct): one question with several picks, its boxes ticked, Send counting them.
    let branches = vec![ask_q("q1", Some("Branches"), true, &[("resampler-old", "merged in September"), ("resampler-v2", "merged in October"), ("meter-fix", "two commits not in main"), ("stash-2024", "untouched since March")])];
    v.push(card(
        "question_multi",
        "Question: several picks",
        "Question card",
        "Multiple select: a box per option, the ticked ones counted on Send.",
        vec![ev(0.1, ask_card("branches", branches, 600_000)), ev(0.6, Event::AskSet { id: "branches".into(), answer: picked("q1", &["resampler-old", "resampler-v2"]) })],
        2.0,
        vec![(0.6, "Two picked")],
    ));
    // Three questions on one card: step chips, the third tab open, the first two answered.
    let three = vec![
        ask_q("q1", Some("Branches"), false, &[("resampler-old", "merged in September"), ("resampler-v2", "merged in October")]),
        ask_q("q2", Some("Push"), false, &[("Yes", "push now"), ("Tomorrow", "after the call"), ("Never", "keep local")]),
        ask_q("q3", Some("Voice"), false, &[("Dave", ""), ("Default", ""), ("None", "")]),
    ];
    v.push(card(
        "question_tabs",
        "Question: three on one card",
        "Question card",
        "One card for every question: a step chip each, the answered ones ticked, the open tab in white.",
        vec![
            ev(0.1, ask_card("tabs", three, 600_000)),
            ev(0.5, Event::AskSet { id: "tabs".into(), answer: picked("q1", &["resampler-v2"]) }),
            ev(0.7, Event::AskSet { id: "tabs".into(), answer: picked("q2", &["Yes"]) }),
            ev(0.9, Event::AskTab { id: "tabs".into(), question: 2 }),
        ],
        2.2,
        vec![(0.9, "Third tab")],
    ));
    v.push(card(
        "question_by_voice",
        "Question answered by voice",
        "Question card",
        "User says no: it retracts at once (Heimdall's voice confirms).",
        vec![ev(0.1, ask("q", "Push the fix to main?", &["Yes", "No"], 600_000)), ev(1.6, Event::CloseQuestion { id: "q".into(), answer: "No.".into() })],
        3.0,
        vec![(1.6, "No")],
    ));
    // The user types his own answer in the Other row: the field holds it, the send arrow lights up.
    v.push(card(
        "question_typing",
        "Question while User types",
        "Question card",
        "User types his own answer in the last row: the arrow lights up.",
        vec![ev(0.1, ask("q", "Push the fix to main?", &["Yes", "No"], 600_000)), ev(0.8, Event::AskOther { id: "q".into(), question: 0, text: "Not yet, after the tests".into() })],
        3.0,
        vec![],
    ));
    // The user talks and Heimdall thinks about it: the card stays up, the thinking dots sit above it (no close with his words).
    v.push(card(
        "question_thinking",
        "Question while Heimdall thinks",
        "Question card",
        "User talks, not an answer: the card stays, the thinking dots sit above it.",
        vec![ev(0.1, ask("q", "Push the fix to main?", &["Yes", "No"], 600_000)), ev(0.8, Event::Activity(Some(Activity::Thinking)))],
        3.0,
        vec![],
    ));
    // Friends (personal edition): a pairing request and an escalated ask, the same question card with its own answers.
    v.push(card(
        "friend_pairing",
        "Friend pairing",
        "Friends",
        "A friend passed the proof for an invite: Allow or Deny.",
        vec![ev(0.1, ask("friends:pair:demo", "Morgan wants to connect", &["Allow", "Deny"], 300_000))],
        1.6,
        vec![(0.1, "Allow")],
    ));
    v.push(card(
        "friend_ask",
        "Friend asks",
        "Friends",
        "A friend's question is not on their shelf: Answer…, Share… or Deny.",
        vec![ev(0.1, ask("friends:ask:demo", "Morgan asks: Is User free Friday?", &["Answer…", "Share…", "Deny"], 900_000))],
        1.6,
        vec![(0.1, "Answer…")],
    ));
    // Share…: the card's answers become the friend's newest shelf files, Back last. The island marks the click as the
    // answer, and the app sends the card again with the list (as it does live).
    v.push(card(
        "friend_share_list",
        "Friend shares a file",
        "Friends",
        "Share… lists the friend's newest files on the card, Back last.",
        vec![
            ev(0.1, ask("friends:ask:demo", "Morgan asks: Is User free Friday?", &["Answer…", "Share…", "Deny"], 900_000)),
            (1.0, Step::ClickAny(1)),
            ev(1.2, ask("friends:ask:demo", "Morgan asks: Is User free Friday?", &["Lisbon packing list and budget (final v3).pdf", "Quarterly plan 2026.pdf", "photo-1.jpg", "notes.md", "hours.txt", "Back"], 900_000)),
        ],
        2.4,
        vec![(1.0, "Share…"), (1.2, "Files")],
    ));
    // Share… on an empty shelf: a line says so, and the card keeps its three answers.
    v.push(card(
        "friend_share_empty",
        "Friend's shelf is empty",
        "Friends",
        "Share… finds nothing on the friend's shelf: a line says so, and the card keeps its three answers.",
        vec![
            ev(0.1, ask("friends:ask:demo", "Morgan asks: Is User free Friday?", &["Answer…", "Share…", "Deny"], 900_000)),
            (1.0, Step::ClickAny(1)),
            ev(1.2, line("friends-line-empty", "Morgan's shelf is empty", NoticeKind::Info)),
            ev(1.2, ask("friends:ask:demo", "Morgan asks: Is User free Friday?", &["Answer…", "Share…", "Deny"], 900_000)),
        ],
        2.4,
        vec![(1.0, "Share…"), (1.2, "Empty")],
    ));
    v.push(card("question_expired", "Question expired", "Question card", "The answers go, the question dims, one white Dismiss.", vec![ev(0.1, ask("q", "Run the full test suite now?", &["Yes", "No"], 1600)), (3.4, Step::ClickAny(0))], 4.4, vec![(1.6, "Expired"), (3.4, "Dismiss")]));
    v.push(card(
        "question_under_status",
        "Question under a status",
        "Question card",
        "A restart arrives while a question waits: one shape, the status row on top.",
        vec![
            ev(0.1, ask("q", "Install the new build?", &["Yes", "No"], 600_000)),
            ev(1.0, Event::Health(HealthCue::Restarting { key: "agent".into(), part: Part::Agent, label: "Restarting agent".into() })),
            ev(3.0, Event::Health(HealthCue::Recovered { key: "agent".into(), part: Part::Agent, label: "Agent back".into() })),
            (5.6, Step::ClickAny(0)),
        ],
        7.0,
        vec![(1.0, "Restarting"), (3.0, "Back"), (5.6, "Yes")],
    ));
    v.push(card(
        "question_one_at_a_time",
        "Two questions, one at a time",
        "Question card",
        "Two questions arrive together: the oldest shows; once answered the next follows in the same shape.",
        vec![ev(0.1, ask("q1", "Install the new build?", &["Yes", "No"], 600_000)), ev(0.15, ask("q2", "Which build tonight?", &["Main", "Island", "Reader"], 600_000)), (1.8, Step::ClickAny(0)), (3.6, Step::ClickAny(2))],
        4.8,
        vec![(0.1, "Two ask"), (1.8, "Yes"), (3.6, "Reader")],
    ));
    let mut connect = AgentNotice::line("t3-connect", "", NoticeKind::Warning);
    connect.action = Some(NoticeAction::ConnectT3);
    v.push(card("connect_t3", "Connect T3", "Question card", "T3 is disconnected: a stack with its white button.", vec![ev(0.1, Event::Notice { notice: connect, body: None }), (2.2, Step::ClickAny(0))], 3.2, vec![(2.2, "Connect")]));
    // Pull-down: the last turn (its reply and its notes) under the notch.
    v.push(card(
        "note_open",
        "Pull-down: a note opens the last turn, pull up, open again",
        "Pull-down",
        "A note arrives with its turn's reply: the pull-down shows both; its lip pulls it up to nothing beside the notch; the notch opens it again.",
        vec![
            ev(0.1, Event::Notice { notice: table_note(), body: Some((300.0, 100.0)) }),
            ev(0.3, Event::LastReply("The tests are faster now. The agent target is still the slowest.".into())),
            ev(2.4, Event::Hover(Some(Target::ReplyBand))),
            (2.8, Step::Ev(Event::Click(Target::ReplyBand))),
            (4.4, Step::Ev(Event::Click(Target::Token(super::Token::Reply)))),
        ],
        5.6,
        vec![(0.1, "Opens"), (2.8, "Pulled up"), (4.4, "Open again")],
    ));
    v.push(card(
        "note_jobs_table",
        "Pull-down: a wide table shows its columns whole",
        "Pull-down",
        "User's 23:11 jobs table: the reply above it, four columns sized to their content (bug note 2026-10-20T23-12-49).",
        vec![
            ev(0.1, Event::Notice { notice: jobs_note(), body: Some((560.0, 100.0)) }),
            ev(0.3, Event::LastReply("Three local AI jobs in Warsaw. BlockWise pays least but is closest to what you do.".into())),
        ],
        1.6,
        vec![],
    ));
    v.push(card(
        "note_wrapped_table",
        "Pull-down: a table wider than the screen allows wraps its cells",
        "Pull-down",
        "Five columns with long notes: the pull-down at its widest, the long cells wrapped, never cut.",
        vec![ev(0.1, Event::Notice { notice: wide_note(), body: Some((656.0, 260.0)) }), ev(0.3, Event::LastReply("Three matches, with what each asks for.".into()))],
        1.6,
        vec![],
    ));
    v.push(card(
        "note_long_table",
        "Pull-down: a long table scrolls",
        "Pull-down",
        "Forty rows: the pull-down stops at half the screen and scrolls; the header and its X scroll away with the table.",
        vec![ev(0.1, Event::Notice { notice: long_note(), body: Some((300.0, 1062.0)) }), (1.4, Step::Ev(Event::Scroll(160.0)))],
        2.4,
        vec![(1.4, "Scroll")],
    ));
    v.push(card(
        "note_close",
        "Pull-down: a note's X closes it for good",
        "Pull-down",
        "The X beside a note's title removes it; the reply stays (bug note 2026-10-20T23-34-14).",
        vec![
            ev(0.1, Event::Notice { notice: jobs_note(), body: Some((560.0, 100.0)) }),
            ev(0.3, Event::LastReply("Three local AI jobs in Warsaw.".into())),
            (1.6, Step::Ev(Event::Click(Target::NoteClose("notice-jobs".into())))),
        ],
        2.8,
        vec![(1.6, "Close")],
    ));
    v.push(card(
        "note_unattended",
        "Pull-down: the agent's note pulls itself up",
        "Pull-down",
        "The pull-down an agent's note opened reads 12 s with nobody at it, then pulls itself up; nothing stays beside the notch (bug notes 2026-10-20T18-41-17, 22-19-52).",
        vec![ev(0.1, Event::Notice { notice: table_note(), body: Some((300.0, 100.0)) })],
        0.1 + super::scene::NOTE_UNATTENDED + 1.2,
        vec![(0.1, "Opens"), (0.1 + super::scene::NOTE_UNATTENDED, "Pulls itself up")],
    ));
    v.push(card(
        "note_turn_moves_on",
        "Pull-down: a later turn replaces the table",
        "Pull-down",
        "The next reply retires the last turn's table: the notch opens the newest reply alone (bug note 2026-10-20T23-34-14).",
        vec![
            ev(0.1, Event::Notice { notice: jobs_note(), body: Some((560.0, 100.0)) }),
            ev(0.2, Event::LastReply("Three local AI jobs in Warsaw.".into())),
            (1.4, Step::Ev(Event::Click(Target::ReplyBand))),
            ev(2.4, Event::LastReply("Done. The CV for BlockWise is in your Downloads folder.".into())),
            (3.0, Step::Ev(Event::Click(Target::Token(super::Token::Reply)))),
        ],
        4.2,
        vec![(0.1, "Table"), (1.4, "Pulled up"), (2.4, "Next reply"), (3.0, "Open")],
    ));
    // Bug note 22 Oct 15:13: two replies shown as text 1.5 s apart (Speaking off). The first keeps the island for its
    // read time, then the second; the notch opens both, newest last (the app sends them together, `bridge::unread`).
    v.push(card(
        "two_replies_queue",
        "Two replies in a row",
        "Pull-down",
        "With Speaking off a second reply waits until the first has had its read time; pulled down, the notch shows both, newest last.",
        vec![
            ev(0.1, line("r1", TWO_REPLIES[0], NoticeKind::Info)),
            ev(0.1, Event::LastReply(TWO_REPLIES[0].into())),
            ev(1.6, line("r2", TWO_REPLIES[1], NoticeKind::Info)),
            ev(1.6, Event::LastReply(TWO_REPLIES.join("\n\n"))),
            (17.0, Step::Ev(Event::Click(Target::Token(super::Token::Reply)))),
        ],
        19.0,
        vec![(0.1, "First reply"), (1.6, "Second waits"), (17.0, "Pull down")],
    ));
    v.push(card("note_error_log", "Error with its log", "Pull-down", "An error header over a monospaced excerpt.", vec![ev(0.1, Event::Notice { notice: log_note(), body: Some((364.0, 90.0)) }), (3.0, Step::Ev(Event::Click(Target::ReplyBand)))], 4.0, vec![]));
    // Night 1022: floating islands and the notch.
    v.push(card("nudge_bell", "Nudge: an island waits", "Floating islands", "A floating island newly waits on User: the bell rings under the notch while the pill jiggles, then it retracts.", vec![ev(0.1, Event::Nudge { question: false })], 2.0, vec![(0.1, "Waiting")]));
    // Switches (24 Oct): ⌃⌘. ⌃⌘/ ⌃⌘, or a tile; the glyph turns in its ear, Listening and Seeing off stay as a token.
    let sw = |switch: Switch, on: bool| Event::Switch { switch, on, moment: true };
    v.push(card("switch_listening_off", "Listening off", "Switches", "⌃⌘.: the mic shows in the left ear and the slash is drawn through it, then it rests dim beside the notch.", vec![ev(0.1, sw(Switch::Listening, false))], 2.6, vec![(0.1, "Off")]));
    v.push(card(
        "switch_listening_on",
        "Listening on",
        "Switches",
        "⌃⌘. again: the slashed mic brightens, the slash lifts, and the island rests.",
        vec![ev(0.1, Event::Switch { switch: Switch::Listening, on: false, moment: false }), ev(0.6, sw(Switch::Listening, true))],
        2.8,
        vec![(0.6, "On")],
    ));
    v.push(card("switch_seeing_off", "Seeing off", "Switches", "⌃⌘/: the eye shows in the right ear and closes under its slash, then it rests dim.", vec![ev(0.1, sw(Switch::Seeing, false))], 2.6, vec![(0.1, "Off")]));
    v.push(card(
        "switch_both_off",
        "Deaf and blind",
        "Switches",
        "Listening off, then Seeing off: the token holds both slashed glyphs.",
        vec![ev(0.1, sw(Switch::Listening, false)), ev(2.0, sw(Switch::Seeing, false))],
        4.2,
        vec![(0.1, "Listening off"), (2.0, "Seeing off")],
    ));
    v.push(card("switch_islands_hidden", "Islands hidden", "Switches", "⌃⌘,: the islands glyph is slashed and a check answers, then the island rests.", vec![ev(0.1, sw(Switch::Islands, false))], 2.4, vec![(0.1, "Hidden")]));
    v.push(card("nudge_question", "Nudge: an island asks", "Floating islands", "A floating island newly asks User something: the blue question mark, the same jiggle.", vec![ev(0.1, Event::Nudge { question: true })], 2.0, vec![(0.1, "Question")]));
    // Dictating to Vella (docs/vella.md): its mark and a few words, then the mark at rest in an ear; off, one motion.
    let vella = |on: bool| Event::Vella { on, keys: Some("⌃⌘N".into()) };
    v.push(card("vella_dictating", "Dictating to Vella", "Vella", "⌃⌘N in Vella: its mark over \"Dictating to Vella\", then the mark rests in the left ear beside the shortcut while User's words cannot wake Heimdall.", vec![ev(0.1, vella(true))], 3.2, vec![(0.1, "Start")]));
    v.push(card("vella_stopped", "Vella stops", "Vella", "Dictation ends: the token goes into the notch in one motion.", vec![ev(0.1, vella(true)), ev(2.6, vella(false))], 3.6, vec![(0.1, "Start"), (2.6, "Stop")]));
    let mut pull = vec![
        ev(0.05, Event::LastReply("**Overnight summary.** Three things landed.\n\n- The voice underrun is fixed: the first sentence is pre-buffered.\n- Floating Islands run inside Heimdall.\n- The menu revamp waits for your review.".into())),
        (0.3, Step::Ev(Event::Click(Target::Token(super::Token::Reply)))),
    ];
    for k in 1..=30 {
        pull.push(ev(1.4 + k as f64 / 60.0, Event::Pull(Some(k as f64 * 3.2))));
    }
    pull.push(ev(2.05, Event::Pull(None)));
    v.push(card("reply_pull_out", "Pull-down: pulled out", "Floating islands", "Dragged down past the threshold, the reply stretches like a rubber band; let go, it leaves as a floating document island and the panel retracts.", pull, 3.0, vec![(0.3, "Open"), (1.4, "Drag"), (2.05, "Let go")]));
    // 23 Oct: the pull-down is the conversation, the last turn at the bottom with its table and pins.
    let history = chat_day();
    v.push(card(
        "chat_history",
        "Pull-down: the conversation",
        "Pull-down",
        "Pulled down, the last turn sits under the conversation that led to it: User's words in blue on the right, Heimdall's replies on the left, quiet action rows; scrolled up, earlier turns.",
        vec![
            ev(0.05, Event::Chat(history.clone())),
            // The agent attaches the table mid-turn; its reply as written ends the turn.
            ev(0.08, Event::Notice { notice: memory_note(), body: Some((300.0, 22.0 + 4.0 * 26.0)) }),
            ev(0.1, Event::LastReply(CHAT_REPLY.into())),
            (0.4, Step::Ev(Event::Click(Target::ReplyBand))),
            (1.4, Step::Ev(Event::Click(Target::Token(super::Token::Reply)))),
            (2.6, Step::Ev(Event::Scroll(-170.0))),
            (3.6, Step::Ev(Event::Scroll(-2000.0))),
        ],
        4.6,
        vec![(1.4, "Pull down"), (2.6, "Scroll up"), (3.6, "Top")],
    ));
    v.push(card(
        "chat_only",
        "Pull-down: conversation only",
        "Pull-down",
        "After a restart, with no turn of its own yet, the notch still opens today's conversation, newest at the bottom.",
        vec![ev(0.05, Event::Chat(history[..history.len() - 2].to_vec())), (0.4, Step::Ev(Event::Click(Target::Token(super::Token::Reply))))],
        1.6,
        vec![(0.4, "Pull down")],
    ));
    // Night 1027 (bug notes 2026-10-26T19-14-48 and 19-14-21-3): the conversation stays open through a send and the
    // thinking, and follows the newest message to the end.
    let mut sent = history.clone();
    sent.push(crate::chat::ChatEntry { at_ms: sent.last().map_or(0, |e| e.at_ms) + 1000, kind: crate::chat::ChatKind::Owner("And what is on the GPU?".into()) });
    v.push(card(
        "chat_send_follows",
        "Pull-down: a send keeps it open",
        "Pull-down",
        "User scrolls up, types and sends: the conversation stays open, scrolls to his words at the bottom, thinks, and follows the reply down as it arrives.",
        vec![
            ev(0.05, Event::Chat(history.clone())),
            ev(0.1, Event::LastReply(CHAT_REPLY.into())),
            ev(0.4, Event::OpenChat),
            (1.4, Step::Ev(Event::Scroll(-170.0))),
            ev(2.0, Event::ComposerSent("And what is on the GPU?".into())),
            ev(2.05, Event::Sent),
            ev(2.1, Event::Chat(sent)),
            ev(2.4, Event::Activity(Some(Activity::Thinking))),
            ev(3.8, Event::LastReply("The decider and the voice, about 3 GB each.\n\nBoth idle between calls; nothing else is loaded.".into())),
            ev(3.9, Event::Activity(None)),
        ],
        5.2,
        vec![(0.4, "⌃⌘H"), (1.4, "Scroll up"), (2.05, "Sent"), (2.4, "Thinking"), (3.8, "Reply")],
    ));
    // Bug note 2026-10-25T23-17-01: a wide table widens the column, and the reply's lines with it.
    let mut wide = jobs_note();
    wide.id = "notice-wide".into();
    v.push(card(
        "reply_wide_table",
        "Pull-down: reply beside a wide table",
        "Pull-down",
        "A table wider than a reply widens the column; the reply above it wraps to the same width.",
        vec![
            ev(0.05, Event::Notice { notice: wide, body: Some((700.0, 22.0 + 5.0 * 26.0)) }),
            ev(0.1, Event::LastReply("Five offers in Warsaw this week, two of them remote. The best paid is the inference role at 32k, and the one closest to your stack is the MLX tooling job, which asks for Rust and Metal.".into())),
        ],
        1.6,
        vec![(0.05, "Table and reply")],
    ));
    // "Whenever you're asking me a question and then you start talking … I should see you talking,
    // and the decision goes down". The card docks under the voice's words; it never replaces them.
    let mut talk = vec![ev(0.1, ask("q-publish", "Publish Vella 2.1.1 now?", &["Publish", "Hold draft"], u64::MAX / 4))];
    talk.extend(speak(0.5, "The signed draft is ready, built with the same Swift as 2.1.0.", 3.2));
    talk.push(ev(2.2, Event::SpokenText(Some("Say the word and it goes to GitHub and Homebrew.".into()))));
    v.push(card(
        "question_while_speaking",
        "Question while speaking",
        "Questions",
        "A question asked while Heimdall talks: his words stay on top as they are spoken, the card with its answers docks below; when the voice ends, the card stays alone.",
        talk,
        6.0,
        vec![(0.1, "Question"), (0.5, "Speaks"), (2.2, "More words"), (4.2, "Voice ends")],
    ));
    v.push(card(
        "question_in_chat",
        "Question in the conversation",
        "Questions",
        "A question arriving while User reads the conversation docks at its bottom, above the band, instead of replacing it.",
        vec![
            ev(0.05, Event::Chat(history.clone())),
            ev(0.08, Event::Notice { notice: memory_note(), body: Some((300.0, 22.0 + 4.0 * 26.0)) }),
            ev(0.1, Event::LastReply(CHAT_REPLY.into())),
            (0.4, Step::Ev(Event::Click(Target::ReplyBand))),
            (1.0, Step::Ev(Event::Click(Target::Token(super::Token::Reply)))),
            ev(2.0, ask("q-retrain", "Pause the retrain to free 38 GB?", &["Pause it", "Keep going"], u64::MAX / 4)),
        ],
        3.2,
        vec![(1.0, "Pull down"), (2.0, "Question")],
    ));
    // Fast reader
    let show = |t: f64| ev(t, Event::Reader(FastReadUpdate::Show(FastRead { id: "r".into(), text: READ_TEXT.into(), title: Some("Build report".into()), wpm: 240 })));
    v.push(card("reader_arrives", "Reader arrives, waits", "Fast reader", "The reader opens with the title, play lit, speed and time left in the ears.", vec![show(0.1)], 1.6, vec![]));
    v.push(card(
        "reader_plays",
        "Reader plays",
        "Fast reader",
        "The red focus point, then each unit (1–3 words) for its own time with its letter on the centre line; the face at the end; it closes itself.",
        vec![show(0.1), (0.9, Step::Ev(Event::Click(Target::ReaderPlay)))],
        16.0,
        vec![(0.9, "Play")],
    ));
    v.push(card(
        "reader_pause",
        "Reader pauses",
        "Fast reader",
        "A click on the words shows the sentence, the read part red.",
        vec![show(0.1), (0.8, Step::Ev(Event::Click(Target::ReaderPlay))), (3.0, Step::Ev(Event::Click(Target::ReaderFace))), (4.6, Step::Ev(Event::Click(Target::ReaderPlay)))],
        5.6,
        vec![(3.0, "Pause"), (4.6, "Play")],
    ));
    v.push(card(
        "reader_pull_up",
        "Reader: pull up, open again",
        "Fast reader",
        "The lip pulls it up into the ears (focus mark and chevron); a click opens it again.",
        vec![show(0.1), (1.8, Step::Ev(Event::Click(Target::ReaderBand))), (3.4, Step::Ev(Event::Click(Target::Token(super::Token::Reader))))],
        4.6,
        vec![(1.8, "Pull up"), (3.4, "Open")],
    ));
    // Drawing mode
    v.push(card(
        "drawing_open",
        "Drawing light: open",
        "Drawing mode",
        "The island grows its keys and lights; the light runs from its flares round the screen on the same clock.",
        vec![ev(0.1, Event::DrawOpen), ev(2.6, Event::Ink(2))],
        3.6,
        vec![(0.1, "⌃⌘M"), (2.6, "Ink")],
    ));
    v.push(card(
        "drawing_send",
        "Drawing light: send",
        "Drawing mode",
        "Hover and press Send: the screen flashes and the keys retract as the light runs back.",
        vec![ev(0.1, Event::DrawOpen), ev(0.5, Event::Ink(3)), ev(1.6, Event::Hover(Some(Target::DrawSend))), ev(2.1, Event::Press(Some(Target::DrawSend))), (2.25, Step::Ev(Event::Click(Target::DrawSend)))],
        5.4,
        vec![(1.6, "Hover"), (2.1, "Press"), (2.25, "Send")],
    ));
    v.push(Card {
        full: true,
        ..card(
            "drawing_full",
            "Drawing light, whole screen",
            "Drawing mode",
            "The whole screen: the light runs from the island round the edge, breathes, flashes on Send and runs back.",
            vec![ev(0.1, Event::DrawOpen), ev(0.5, Event::Ink(3)), (2.4, Step::Ev(Event::Click(Target::DrawSend)))],
            4.0,
            vec![(0.1, "Open"), (2.4, "Send")],
        )
    });
    v.push(card("drawing_close", "Drawing light: close", "Drawing mode", "Esc: the light runs back into the island while it retracts.", vec![ev(0.1, Event::DrawOpen), (1.8, Step::Ev(Event::Click(Target::DrawClose)))], 3.0, vec![(1.8, "Esc")]));
    v.push(card(
        "drawing_look",
        "Look at this while drawing",
        "Drawing mode",
        "The keys stay (Send never leaves); for a second the ears show eye and check.",
        vec![
            ev(0.1, Event::DrawOpen),
            ev(0.4, Event::Ink(1)),
            ev(1.2, Event::Command(CommandCue::Heard { id: 1, kind: CommandKind::LookAtThis })),
            ev(1.6, Event::Command(CommandCue::Done { id: Some(1), kind: CommandKind::LookAtThis, label: "Screenshot".into() })),
        ],
        4.0,
        vec![(1.6, "Screenshot")],
    ));
    // Pointing
    let mut point = speak(0.1, "This one.", 2.2);
    point.push(ev(0.3, Event::Point(2)));
    v.push(card("pointing", "Pointing", "Pointing", "Speaking with words while blue marks show; the marks fade with the voice.", point.clone(), 3.4, vec![(0.3, "Marks"), (2.3, "Voice ends")]));
    v.push(Card { full: true, ..card("pointing_full", "Pointing, whole screen", "Pointing", "The whole screen: blue marks arrive with the voice and fade when it ends.", point, 3.4, vec![(0.3, "Marks"), (2.3, "Voice ends")]) });
    // Health
    v.push(card(
        "health_restart_ok",
        "Restarting, then it leaves",
        "Health",
        "Orange restart (rotating while held); when the part is back it simply leaves.",
        vec![
            ev(0.1, Event::Health(HealthCue::Restarting { key: "listen".into(), part: Part::Transcription, label: "Restarting transcription".into() })),
            ev(2.2, Event::Health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription back".into() })),
        ],
        4.6,
        vec![(0.1, "Restarting"), (2.2, "Back")],
    ));
    // 31 Oct: the archive was locked about four minutes and speech stalled with no sign. Heimdall says it is catching
    // up, in calm white; the app resends the count every few seconds and only the words change; then it rests as a
    // token and clears by itself.
    v.push(card(
        "health_catching_up",
        "Catching up, then it clears",
        "Health",
        "White stack, its layers lighting in turn, with what it does; the count falls in place, then it rests in the ears and leaves.",
        vec![
            ev(0.1, Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: "Catching up · 6,489 notes to save".into() })),
            ev(2.1, Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: "Catching up · 4,920 notes to save".into() })),
            ev(4.1, Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: "Catching up · 2,310 notes to save".into() })),
            ev(6.1, Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: "Catching up · 410 notes to save".into() })),
            ev(7.4, Event::Health(HealthCue::Recovered { key: "archive".into(), part: Part::Agent, label: "Caught up".into() })),
        ],
        9.0,
        vec![(0.1, "Catching up"), (2.1, "Count falls"), (4.6, "Token"), (7.4, "Back")],
    ));
    v.push(card(
        "health_catching_up_restart",
        "A restart over catching up",
        "Health",
        "Catching up rests in the ears; a restart takes the island and its token; back, catching up's token returns, then it clears.",
        vec![
            ev(0.1, Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: "Catching up · 6,489 notes to save".into() })),
            ev(4.6, Event::Health(HealthCue::Restarting { key: "listen".into(), part: Part::Transcription, label: "Restarting transcription".into() })),
            ev(6.6, Event::Health(HealthCue::Busy { key: "archive".into(), part: Part::Agent, label: "Catching up · 1,204 notes to save".into() })),
            ev(8.2, Event::Health(HealthCue::Recovered { key: "listen".into(), part: Part::Transcription, label: "Transcription back".into() })),
            ev(10.4, Event::Health(HealthCue::Clear { key: "archive".into() })),
        ],
        12.0,
        vec![(0.1, "Catching up"), (4.6, "Restart"), (8.2, "Restart back"), (10.4, "Clears")],
    ));
    // Bug notes 26 Oct 00:52:29, 00:54:39: a wake whose model request keeps timing out (crates/app bridge/startup.rs
    // `resolution_cues`): orange while it waits to retry, then red when the resends are spent.
    v.push(card(
        "health_agent_retry",
        "Agent retrying, then giving up",
        "Health",
        "The model does not answer: orange with when the next try goes, the try itself, then red when Heimdall gives up.",
        vec![
            ev(0.1, Event::Health(HealthCue::Restarting { key: "agent-retry".into(), part: Part::Agent, label: "Model not answering · retrying in 15 s".into() })),
            ev(2.4, Event::Health(HealthCue::Restarting { key: "agent-retry".into(), part: Part::Agent, label: "Model not answering · retrying now".into() })),
            ev(4.8, Event::Health(HealthCue::Problem { key: "agent-retry".into(), part: Part::Agent, label: "Model not answering · gave up".into(), action: "Say it again".into(), control: HealthControl::OpenAgent })),
        ],
        8.0,
        vec![(0.1, "Retrying in"), (2.4, "Retrying now"), (4.8, "Gave up")],
    ));
    v.push(card(
        "health_problem",
        "Problem",
        "Health",
        "Red and what to do with a white button; after its read time it rests as a token; a click reopens it.",
        vec![
            ev(0.1, Event::Health(HealthCue::Problem { key: "screen".into(), part: Part::Screen, label: "Screen Recording off".into(), action: "Open Settings".into(), control: HealthControl::OpenSettings(Permission::ScreenRecording) })),
            (7.0, Step::Ev(Event::Click(Target::Token(super::Token::Problem)))),
            ev(9.0, Event::Health(HealthCue::Clear { key: "screen".into() })),
        ],
        10.0,
        vec![(0.1, "Problem"), (6.1, "Token"), (7.0, "Click"), (9.0, "Fixed")],
    ));
    v.push(card(
        "health_mic_switched",
        "Microphone switched",
        "Health",
        "AirPods dropped, the built-in mic took over.",
        vec![ev(0.1, Event::Health(HealthCue::Drop { key: "mic".into(), part: Part::Microphone, label: "AirPods disconnected".into() })), ev(3.2, Event::Health(HealthCue::Clear { key: "mic".into() }))],
        4.2,
        vec![(0.1, "Dropped"), (3.2, "Back")],
    ));
    // Emergency mode (docs/emergency.md): the agent is down and a backup agent covers; said once, nothing at rest, then
    // "Agent back" when it answers again.
    v.push(card(
        "health_emergency",
        "Agent down, backup covering",
        "Health",
        "The agent's quota ran out: why, that a backup agent is on it, nothing to do; later the agent is back.",
        vec![
            ev(0.1, Event::Health(HealthCue::Notice { part: Part::Agent, label: "Agent out of quota till 03:04 · backup agent on it, nothing to do".into() })),
            ev(5.0, Event::Health(HealthCue::Recovered { key: "emergency".into(), part: Part::Agent, label: "Agent back".into() })),
        ],
        7.0,
        vec![(0.1, "Down"), (3.4, "At rest"), (5.0, "Back")],
    ));
    // Emergency mode at rest (bug note 27 Oct 19:40, "The emergency triangle next to the notch never showed"): the
    // notice reads, then the agent's glyph and the warning triangle rest beside the notch until it ends.
    v.push(card(
        "health_emergency_rest",
        "Emergency mode, at rest",
        "Health",
        "While a backup agent covers, the agent's glyph and a warning triangle stay beside the notch; they go when the agent is back.",
        vec![
            ev(0.1, Event::Emergency(true)),
            ev(0.1, Event::Health(HealthCue::Notice { part: Part::Agent, label: "Agent out of quota till 03:04 · backup agent on it, nothing to do".into() })),
            ev(6.0, Event::Emergency(false)),
            ev(6.0, Event::Health(HealthCue::Recovered { key: "emergency".into(), part: Part::Agent, label: "Agent back".into() })),
        ],
        8.0,
        vec![(0.1, "Down"), (4.5, "At rest"), (7.5, "Back")],
    ));
    // Nothing can cover (crates/app/src/emergency_local.rs, its words): the triangle says why and reads alone, then the
    // one question (Yes / No, no Other row; the download's needs under Yes).
    let no_relay = || line("emergency-local:warning", "Quota ran out · no T3 Code relay can drive Heimdall", NoticeKind::Warning);
    let yes_no = |id: &str, text: &str, needs: Option<&str>| {
        let q = AskQuestion { id: "q1".into(), header: None, text: text.into(), multi_select: false, allow_other: false, field: None,
            options: vec![AskOption { label: "Yes".into(), description: needs.map(str::to_owned), grant: vec![] }, AskOption::label("No")] };
        let mut n = AgentNotice::line(id, text, NoticeKind::Warning);
        n.action = Some(NoticeAction::Ask { request_id: id.into(), questions: vec![q], expires_ms: 900_000, countdown: false });
        Event::Notice { notice: n, body: None }
    };
    v.push(card("no_relay_local", "No relay: the local model", "Health", "No T3 relay can cover and the local model is here: switch to it?",
        vec![ev(0.1, no_relay()), ev(3.6, yes_no("emergency-local:1", "Switch to the local model, Qwen3.8 27B?", None))], 9.0,
        vec![(1.5, "Why"), (7.5, "Question")]));
    v.push(card("no_relay_download", "No relay: download the local model", "Health", "The engine is here, the model is not: whether to download and run it, its needs under Yes.",
        vec![ev(0.1, no_relay()), ev(3.6, yes_no("emergency-local:2", "Download the local model, Qwen3.8 27B?", Some("16.8 GB download · up to 48 GB memory · 100 GB disk")))], 9.0,
        vec![(1.5, "Why"), (7.5, "Question")]));
    v.push(card("no_relay_waiting", "No relay: waiting for the quota", "Health", "User says No: a quiet line with the reset, then the emergency token rests.",
        vec![ev(0.05, Event::Emergency(true)), ev(0.1, no_relay()), ev(3.6, yes_no("emergency-local:3", "Switch to the local model, Qwen3.8 27B?", None)), (7.5, Step::ClickAny(1)),
            ev(7.7, Event::NoticeClear("emergency-local:3".into())), ev(7.8, line("emergency-local:waiting", "Waiting for the quota · back at 21:15", NoticeKind::Info))], 14.0,
        vec![(8.5, "Waiting"), (13.5, "At rest")]));
    // Without a notch
    let plain = |c: Card| Card { scenario: c.scenario.on(Display::PLAIN), ..c };
    v.push(plain(card("no_notch_thinking", "Thinking, no notch", "Without a notch", "A black band stands in for the notch while the island shows; nothing at rest.", vec![ev(0.1, Event::Sent), ev(0.3, Event::Activity(Some(Activity::Thinking))), ev(2.6, Event::Activity(None))], 3.4, vec![])));
    v.push(plain(card(
        "no_notch_screenshot",
        "Screenshot, no notch",
        "Without a notch",
        "The same template and sizes.",
        vec![ev(0.1, Event::Command(CommandCue::Done { id: None, kind: CommandKind::Screenshot, label: "Screenshot".into() }))],
        3.0,
        vec![],
    )));
    // Bug note 2026-10-28T19-45-55, review: nothing could bring a tucked card back without a notch, so no lip there.
    v.push(plain(card("no_notch_question", "Question, no notch", "Without a notch", "The card without its lip: there is no notch to bring it back.", vec![ev(0.1, ask("q", "Push the fix to main?", &["Yes", "No"], 600_000))], 2.0, vec![])));
    v
}
