//! Bug note 30 Oct 03:00 (the user): "In relay mode the island pull-down shows nothing while the relay agent is working:
//! no thinking/working indicator and none of its activity (tool calls, speak lines) the way Pi turns show."
//!
//! One scenario, a Pi turn and a relay turn side by side through the real pieces: the agent's window state (what the
//! supervisor publishes), the island's activity it maps to (`ui::Activity::of`), the pull-down's conversation rows
//! (`chat_history::History`: Pi's from its session file, the relay's from `emergency::Own`'s live calls), and the
//! island model the user pulls down.
use super::*;
use crate::chat_history::{day_start, History};
use serde_json::json;
use ui::chat::{ChatEntry, ChatKind};
use ui::island::object::{Approx, Content, Display, Event as IslandEvent, Island, Scene};

const DAY: u64 = 1_792_706_400_000;
fn at(sec: u64) -> u64 { day_start(DAY) + 15 * 3_600_000 + sec * 1000 }

/// The turn's Heimdall tool calls and their results, as both agents make them.
fn calls() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        ("speak", json!({"text":"On it, checking the build."}), "Speech queued: 1."),
        ("island_notice", json!({"text":"Build","detail":{"type":"text","text":"cargo test: 412 passed"}}), "Notice shown: notice-build."),
        ("delegate_task", json!({"title":"Fix the slow test","target":{"model":"claude-opus-5-5"}}), "Started task 'Fix the slow test' (task-7)."),
        ("ask", json!({"question":"Push it to main?"}), "Asked: ask-42. User's answer arrives as a notice."),
    ]
}
const TYPED: &str = "Is the build green?";
const REPLY: &str = "Green: 412 tests passed. A worker is fixing the slow one.";

/// Pi's turn as its session file records it.
fn pi_rows() -> Vec<ChatEntry> {
    let mut h = History::default();
    h.entry(&json!({"type":"message","message":{"role":"user","timestamp":at(0),"content":[{"type":"text","text":format!("[Heimdall delivery: a]\n[Heimdall typed message]\n{TYPED}")}]}}), day_start(DAY));
    for (k, (name, args, result)) in calls().into_iter().enumerate() {
        let id = format!("t{k}");
        h.entry(&json!({"type":"message","message":{"role":"assistant","timestamp":at(1 + k as u64),"stopReason":"toolUse","content":[{"type":"toolCall","id":id,"name":format!("mcp__heimdall__{name}"),"arguments":args}]}}), day_start(DAY));
        h.entry(&json!({"type":"message","message":{"role":"toolResult","timestamp":at(1 + k as u64),"toolCallId":id,"isError":false,"content":[{"type":"text","text":result}]}}), day_start(DAY));
    }
    h.entry(&json!({"type":"message","message":{"role":"assistant","timestamp":at(9),"stopReason":"stop","content":[{"type":"text","text":REPLY}]}}), day_start(DAY));
    h.rows(day_start(DAY))
}

fn custom(ms: u64, text: &str) -> Value {
    let iso = chrono::DateTime::from_timestamp_millis(ms as i64).unwrap().to_rfc3339();
    json!({"type":"custom_message","timestamp":iso,"content":text})
}

/// The relay's turn: the user's words held in Pi's session, each Heimdall tool call live from `Own::record`, then (later,
/// through the outbox) the `said to` record of its speak line, and the mirrored final reply at the run's end.
fn relay_rows() -> Vec<ChatEntry> {
    let mut h = History::default();
    h.entry(&custom(at(0), &format!("{}\n[Heimdall delivery: a]\n[Heimdall typed message]\n{TYPED}", agent::emergency::HELD)), day_start(DAY));
    for (k, (name, args, result)) in calls().into_iter().enumerate() {
        h.relay_call(name, &args, &Ok(result.to_string()), at(1 + k as u64));
    }
    let record = format!("[Heimdall notice: {}rec-1]\n[Relay agent] said to User: \"On it, checking the build.\" → Speech queued: 1.", agent::notice::EMERGENCY_COVER);
    h.entry(&custom(at(8), &record), day_start(DAY));
    let mirror = format!("[Heimdall notice: {}mirror:thread-1:5:0]\n[Relay agent] {REPLY}", agent::notice::EMERGENCY_COVER);
    h.entry(&custom(at(9), &mirror), day_start(DAY));
    h.rows(day_start(DAY))
}

/// The relay's tool calls make exactly the rows Pi's make: the speak line once (its later record adds nothing), the
/// note, the started task, the question, and the reply.
#[test]
fn relay_tool_calls_make_the_same_rows_as_pi() {
    let pi = pi_rows();
    let kinds: Vec<&str> = pi.iter().map(|e| match &e.kind {
        ChatKind::Owner(_) => "owner", ChatKind::Heimdall { .. } => "heimdall", ChatKind::Action { .. } => "action", ChatKind::Note { .. } => "note",
    }).collect();
    assert_eq!(kinds, ["owner", "heimdall", "note", "action", "action", "heimdall"], "Pi's turn: {pi:#?}");
    assert_eq!(relay_rows(), pi);
}

/// A relay `speak` said again in a later run still shows: each record spends only the line it stands for.
#[test]
fn a_relay_line_said_again_later_still_shows() {
    let mut h = History::default();
    let said = |h: &mut History, ms: u64| {
        h.relay_call("speak", &json!({"text":"Done."}), &Ok("Speech queued: 1.".into()), ms);
        h.entry(&custom(ms + 20_000, &format!("[Heimdall notice: {}r{ms}]\n[Relay agent] said to User: \"Done.\" → Speech queued: 1.", agent::notice::EMERGENCY_COVER)), day_start(DAY));
    };
    said(&mut h, at(0));
    h.entry(&custom(at(60), &format!("{}\n[Heimdall typed message]\nAgain?", agent::emergency::HELD)), day_start(DAY));
    said(&mut h, at(120));
    let lines = h.rows(day_start(DAY)).iter().filter(|e| matches!(&e.kind, ChatKind::Heimdall { text, .. } if text == "Done.")).count();
    assert_eq!(lines, 2);
}

fn scene_working(i: &Island) -> bool {
    matches!(i.scene(), Scene::Reply { working: true, .. })
}
fn dots(i: &Island, t: f64) -> bool {
    i.frame(t).slots.iter().any(|s| &*s.id == "reply.working" && matches!(&*s.content, Content::Dots))
}

/// The pull-down while the turn runs and after it ends, for a Pi turn and a relay turn: the same scene throughout.
fn pull_down(window: &[ui::AgentRunState], rows: Vec<ChatEntry>) -> Vec<(bool, bool, Scene)> {
    let mut i = Island::new(Display::MBP16, Box::new(Approx));
    let mut seen = vec![];
    let owner = rows[..1].to_vec();
    i.apply(IslandEvent::Chat(owner), 0.0);
    i.apply(IslandEvent::OpenChat, 0.1);
    i.apply(IslandEvent::Sent, 0.2);
    let mut t = 0.3;
    for (k, state) in window.iter().enumerate() {
        i.apply(IslandEvent::Activity(ui::Activity::of(state)), t);
        if k == 0 {
            // Mid-run: the tool calls' rows arrive.
            i.apply(IslandEvent::Chat(rows[..rows.len() - 1].to_vec()), t + 0.1);
        } else {
            i.apply(IslandEvent::LastReply(REPLY.into()), t);
            i.apply(IslandEvent::Chat(rows.clone()), t + 0.1);
        }
        for _ in 0..12 {
            t += 0.25;
            i.apply(IslandEvent::Tick, t);
        }
        seen.push((scene_working(&i), dots(&i, t), i.scene().clone()));
    }
    seen
}

/// The working indicator: a relay run shows Working in the agent's state exactly as a Pi turn does, the island thinks,
/// and the pulled-down conversation shows the thinking dots under the relay's rows; when the run ends it is Ready and
/// the dots go. Pi's turn and the relay's give the same pull-down at each step.
#[tokio::test]
async fn a_relay_run_shows_working_and_its_rows_in_the_pull_down_then_ready() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let pi = Pi::launch_config(PiConfig::fixture(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py"),
        dir.path().join("session.jsonl"), dir.path().to_owned()))?;
    let mut b = agent::Bridge::open(RecordedAdapter {
        inner: Adapter::Pi(pi), journal: heimdall_archive::Journal::open(&dir.path().join("archive/session-relay"))?,
        gate: None, ids: Default::default(), queries: HashMap::new(),
    }, dir.path().join("spool"))?;
    let settings = Settings { dry_run: false, ..Default::default() };
    // Pi is held and idle while the relay acts: the relay's run alone makes the agent Working.
    let working = relay_window_state(&b, &settings, 0, None, None, true).state;
    let ready = relay_window_state(&b, &settings, 0, None, None, false).state;
    b.shutdown().await?;
    assert_eq!(working, ui::AgentRunState::Working);
    assert_eq!(ready, ui::AgentRunState::Ready);
    assert_eq!(ui::Activity::of(&working), Some(ui::Activity::Thinking));
    assert_eq!(ui::Activity::of(&ready), None);

    let relay = pull_down(&[working.clone(), ready.clone()], relay_rows());
    let pi = pull_down(&[ui::AgentRunState::Working, ui::AgentRunState::Ready], pi_rows());
    assert!(relay[0].0 && relay[0].1, "mid-run the pull-down shows the working dots: {:?}", relay[0].2);
    let Scene::Reply { chat, .. } = &relay[0].2 else { panic!("pulled down: {:?}", relay[0].2) };
    let shown: Vec<&ChatKind> = chat.before.iter().chain(chat.after.iter()).collect();
    assert!(shown.iter().any(|k| matches!(k, ChatKind::Heimdall { text, .. } if text == "On it, checking the build.")), "the speak line: {shown:?}");
    assert!(shown.iter().any(|k| matches!(k, ChatKind::Note { id, .. } if id == "notice-build")), "the note: {shown:?}");
    assert!(shown.iter().any(|k| matches!(k, ChatKind::Action { text, .. } if text == "Fix the slow test")), "the task: {shown:?}");
    assert!(!relay[1].0 && !relay[1].1, "ended, the dots go: {:?}", relay[1].2);
    assert!(matches!(&relay[1].2, Scene::Reply { paragraphs, .. } if paragraphs.join(" ") == REPLY), "the reply shows: {:?}", relay[1].2);
    for (r, p) in relay.iter().zip(&pi) {
        assert_eq!((r.0, r.1, format!("{:?}", r.2)), (p.0, p.1, format!("{:?}", p.2)), "a relay turn's pull-down is a Pi turn's");
    }
    Ok(())
}

/// Review of this fix: the outbox can deliver a line's record before the reader takes its live call (both in one
/// look); the line still shows once, and a failed relay `speak` shows none, as Pi's failed call shows none.
#[test]
fn a_record_read_before_its_live_call_and_a_failed_speak_add_no_rows() {
    let long = "x".repeat(400);
    let mut h = History::default();
    for (k, words) in ["Done.", long.as_str()].into_iter().enumerate() {
        let bounded = if words.chars().count() > 300 { format!("{}…", &words[..300]) } else { words.to_string() };
        let ms = at(10 * k as u64);
        h.entry(&custom(ms + 500, &format!("[Heimdall notice: {}r{k}]\n[Relay agent] said to User: \"{bounded}\" → Speech queued: 1.", agent::notice::EMERGENCY_COVER)), day_start(DAY));
        h.relay_call("speak", &json!({"text":words}), &Ok("Speech queued: 1.".into()), ms);
    }
    h.relay_call("speak", &json!({"text":"Lost."}), &Err("speech unavailable".into()), at(30));
    h.entry(&custom(at(31), &format!("[Heimdall notice: {}r9]\n[Relay agent] said to User: \"Lost.\" → failed: speech unavailable", agent::notice::EMERGENCY_COVER)), day_start(DAY));
    let lines: Vec<String> = h.rows(day_start(DAY)).into_iter().filter_map(|e| match e.kind { ChatKind::Heimdall { text, .. } => Some(text), _ => None }).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0], "Done.");
    assert!(lines[1].starts_with("xxx"));
}
