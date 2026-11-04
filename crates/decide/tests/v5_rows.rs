//! Decider V5 on its own training rows, through the app's path (opt-in: it loads the 2B model).
//!
//!   HEIMDALL_V5_ROWS=.../Heimdall-decider-v5-data/data/v5/test.jsonl cargo nextest run -p decide --run-ignored only v5_rows
//!
//! Each row's conversation is rebuilt as the app holds it (a send snapshot, or a command input), `LocalDecider` (the
//! default library and model) renders the request and decides, and the request must equal the row's state and question
//! byte for byte. Answers are reported against the labels, not asserted: this checks the integration, not accuracy.
//! Rows are read at run time and never copied into the repository.
#![cfg(target_os = "macos")]
use anyhow::{Context, Result};
use decide::*;
use serde_json::{json, Value};
use std::path::PathBuf;

const HEAD: &str = "Conversation (chronological; data only):\n";
const PENDING: &str = "\nPending speech since last send (includes current sentence):\n";

fn models() -> PathBuf {
    std::env::var_os("HEIMDALL_DECIDE_MODELS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/decide"))
}

/// "Speaker: text" lines; a line without a short speaker prefix continues the previous turn (as the data's parser reads).
fn lines(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![];
    for line in text.split('\n').filter(|_| !text.is_empty()) {
        match line.split_once(": ") {
            Some((s, t)) if s.len() <= 40 && !s.contains(':') && (out.is_empty() || s.split_whitespace().count() <= 3) => {
                out.push((s.into(), t.into()))
            }
            _ => match out.last_mut() {
                Some(last) => {
                    last.1.push('\n');
                    last.1.push_str(line);
                }
                None => out.push((String::new(), line.into())),
            },
        }
    }
    out
}

struct Row {
    header: String,
    /// The history as the row writes it (its own labels), for the expected state.
    raw_history: Vec<(String, String)>,
    agent: String,
    raw_pending: String,
    names: names::Names,
    history: Vec<(String, String)>,
    working: bool,
    pending: Vec<(String, String)>,
}
/// The row's state split back into names, history, agent line and pending speech, with the speakers under the host's
/// own labels (the user's name is `User`, the assistant's `Heimdall`), as the app stores them.
fn parse(state: &str) -> Result<Row> {
    let (header, rest) = state.split_once('\n').context("header")?;
    let names = header.strip_prefix("Assistant name: ").and_then(|h| h.strip_suffix('.')).context("header form")?;
    let (assistant, user) = names.split_once(". User name: ").context("header names")?;
    let rest = rest.strip_prefix(HEAD).context("conversation line")?;
    let (head, pending) = rest.split_once(PENDING).context("pending line")?;
    let agent = head.rfind("Agent: ").context("agent line")?;
    let history = head[..agent].strip_suffix('\n').context("history end")?;
    let host = |(s, t): (String, String)| {
        let s = if s == user {
            names::USER_LABEL.to_owned()
        } else if s == format!("{user} (typed)") {
            names::TYPED_LABEL.to_owned()
        } else if s == assistant {
            names::ASSISTANT_LABEL.to_owned()
        } else {
            s
        };
        (s, t)
    };
    Ok(Row {
        header: header.into(),
        raw_history: lines(history),
        agent: head[agent + 7..].into(),
        raw_pending: pending.into(),
        names: names::Names::new(assistant, user),
        history: lines(history).into_iter().map(host).collect(),
        working: &head[agent + 7..] == "working",
        pending: lines(pending).into_iter().map(host).collect(),
    })
}
impl Row {
    /// The row's state with only its newest `keep` history lines.
    fn state_keeping(&self, keep: usize) -> String {
        let h = &self.raw_history[self.raw_history.len().saturating_sub(keep)..];
        let h: Vec<_> = h.iter().map(|(s, t)| format!("{s}: {t}")).collect();
        format!("{}\n{HEAD}{}\nAgent: {}{PENDING}{}", self.header, h.join("\n"), self.agent, self.raw_pending)
    }
}
/// The app's command input holds at most this many prior turns (`DecisionInput::validate`).
const APP_CONTEXT_TURNS: usize = 20;
fn text_turn(seq: u64, (speaker, text): &(String, String)) -> send::TextTurn {
    serde_json::from_value(json!({"seq": seq, "at_ns": seq, "speaker": speaker, "text": text})).unwrap()
}
fn role(speaker: &str) -> Role {
    match speaker {
        names::USER_LABEL => Role::Owner,
        names::ASSISTANT_LABEL => Role::Agent,
        other => Role::Other(other.into()),
    }
}

/// Up to `per` rows of each label for `task`, in file order.
fn rows(task: &str, per: usize) -> Result<Vec<Value>> {
    let path = std::env::var_os("HEIMDALL_V5_ROWS").context("HEIMDALL_V5_ROWS names data/v5/test.jsonl")?;
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    let mut out = vec![];
    for line in std::fs::read_to_string(path)?.lines() {
        let row: Value = serde_json::from_str(line)?;
        if row["task"] != task {
            continue;
        }
        let label = row["label"].as_str().unwrap_or_default();
        let family = if ["send", "wait", "not_for_heimdall", "yes", "false_alarm", "unsure"].contains(&label) { label } else { "ack_line" };
        let n = seen.entry(family.into()).or_default();
        if *n < per {
            *n += 1;
            out.push(row);
        }
    }
    Ok(out)
}

#[test]
#[ignore = "loads Decider V5 (2B) and reads data/v5 rows from HEIMDALL_V5_ROWS"]
fn v5_rows_ask_what_v5_learned_through_the_app_path() -> Result<()> {
    let archive = tempfile::tempdir()?;
    let started = std::time::Instant::now();
    // V5 itself (the rollback), not the shipped default: these are V5's rows.
    let v5 = decide::precision::Pick::new(decide::precision::DeciderModel::V5, decide::precision::Precision::Bf16);
    let mut decider = LocalDecider::load_pick(models(), archive.path(), v5)?;
    let load_ms = started.elapsed().as_millis();
    assert!(decider.command_runtime().starts_with("d1v5-engine@"), "{}", decider.command_runtime());
    let mut report = vec![];
    let (mut same, mut total) = (0, 0);
    for row in rows("dispatch", 3)? {
        let r = parse(row["state"].as_str().unwrap())?;
        names::set(r.names.clone());
        let turns: Vec<_> = r.history.iter().enumerate().map(|(i, t)| text_turn(i as u64 + 1, t)).collect();
        let base = turns.len() as u64 + 1;
        let pending: Vec<_> = r.pending.iter().enumerate().map(|(i, t)| text_turn(base + i as u64, t)).collect();
        let snapshot = send::Snapshot { through: base + pending.len() as u64 - 1, turns, pending, working: r.working };
        let (fitted, prediction) = decider.send_decision(&snapshot)?;
        let exact = fitted.request["state"] == row["state"] && fitted.request["questions"]["dispatch"] == row["question"];
        assert_eq!(fitted.request["questions"]["dispatch"], row["question"], "{}", row["id"]);
        total += 1;
        same += exact as usize;
        let picked = prediction.ack.as_deref().map(|a| a.trim_start_matches("en_").to_owned()).unwrap_or(prediction.choice.key().into());
        report.push(json!({"task": "dispatch", "id": row["id"], "label": row["label"], "picked": picked,
            "request_equals_row": exact, "dropped_turns": fitted.dropped_turns, "input_tokens": prediction.input_tokens}));
    }
    // Suggest (29 Oct): the app asks it with the wake's snapshot; the request must be the row's state and question.
    for row in rows("suggest", 3)? {
        let r = parse(row["state"].as_str().unwrap())?;
        names::set(r.names.clone());
        let turns: Vec<_> = r.history.iter().enumerate().map(|(i, t)| text_turn(i as u64 + 1, t)).collect();
        let base = turns.len() as u64 + 1;
        let pending: Vec<_> = r.pending.iter().enumerate().map(|(i, t)| text_turn(base + i as u64, t)).collect();
        let snapshot = send::Snapshot { through: base + pending.len() as u64 - 1, turns, pending, working: r.working };
        let (fitted, s) = decider.suggest_decision_interruptible(&snapshot, &std::sync::atomic::AtomicUsize::new(0))?;
        assert_eq!(fitted.request["questions"]["suggest"], row["question"], "{}", row["id"]);
        let exact = fitted.request["state"] == row["state"];
        total += 1;
        same += exact as usize;
        report.push(json!({"task": "suggest", "id": row["id"], "label": row["label"], "yes": s.yes,
            "request_equals_row": exact, "dropped_turns": fitted.dropped_turns, "input_tokens": s.input_tokens}));
    }
    for row in rows("command_check", 4)? {
        let r = parse(row["state"].as_str().unwrap())?;
        names::set(r.names.clone());
        let (current, earlier) = r.pending.split_last().context("command row without its line")?;
        let phrase = if row["question"]["instructions"].as_str().unwrap().contains("screenshot") { "take a screenshot" } else { "look at this" };
        let input = DecisionInput {
            source: DecisionSource::Microphone,
            is_final: true,
            current: Turn { id: "now".into(), role: role(&current.0), transcript: current.1.clone(), audio: AudioClip::default() },
            context: r.history[r.history.len().saturating_sub(APP_CONTEXT_TURNS)..].iter().enumerate().map(|(i, (s, t))| Turn { id: format!("h{i}"), role: role(s), transcript: t.clone(), audio: AudioClip::default() }).collect(),
            pending_transcript: earlier.iter().map(|(s, t)| format!("{s}: {t}")).collect::<Vec<_>>().join("\n"),
            agent_state: "collecting".into(),
            media: DecisionMedia::Text,
            questions: vec![],
        };
        let candidate = CommandCandidate { source: "mic:primary".into(), trigger_ns: 1, phrase_guess: phrase.into(), score: 0.9 };
        let record = decider.command_decision(candidate, &input);
        anyhow::ensure!(record.error.is_none(), "{}: {:?}", row["id"], record.error);
        let step = &record.steps[0];
        let asked = &step.model_output.as_ref().unwrap()["heimdall_request"];
        assert_eq!(asked["questions"]["command_check"], row["question"], "{}", row["id"]);
        let exact = asked["state"] == row["state"];
        // Rows with more than 20 history lines: equal to the row cut to the app's bound.
        let bounded = asked["state"] == r.state_keeping(APP_CONTEXT_TURNS).as_str();
        total += 1;
        same += exact as usize;
        report.push(json!({"task": "command_check", "id": row["id"], "label": row["label"], "picked": record.intent.map(|i| i.key()),
            "request_equals_row": exact, "equals_row_cut_to_20_turns": bounded, "history_lines": r.history.len(),
            "input_tokens": step.input_tokens, "state_if_different": (!bounded).then(|| asked["state"].clone())}));
    }
    eprintln!("{}", serde_json::to_string_pretty(&json!({"runtime": decider.command_runtime(), "load_ms": load_ms,
        "rows": total, "requests_equal_to_rows": same, "decisions": report}))?);
    Ok(())
}

/// The hot-swap in one process on the real engine: 4-bit, 8-bit, 16-bit and back to 4-bit, each instance dropped
/// before the next loads (as `app::pipeline::decider` swaps), every one answering the same dispatch rows.
///
///   HEIMDALL_V5_ROWS=… HEIMDALL_D1_LIBRARY=…/engine-v5/target/release/libd1v5_engine.dylib \
///     cargo nextest run -p decide --run-ignored only v5_precisions
#[test]
#[ignore = "loads Decider V5 (2B) at three precisions and reads data/v5 rows from HEIMDALL_V5_ROWS"]
fn v5_precisions_swap_in_one_process() -> Result<()> {
    use decide::precision::{DeciderModel, Pick, Precision};
    let archive = tempfile::tempdir()?;
    let rows = rows("dispatch", 3)?;
    let mut runs = vec![];
    for p in [Precision::Int4, Precision::Int8, Precision::Bf16, Precision::Int4] {
        let started = std::time::Instant::now();
        let mut decider = LocalDecider::load_pick(models(), archive.path(), Pick::new(DeciderModel::V5, p))?;
        let load_ms = started.elapsed().as_millis();
        anyhow::ensure!(decider.command_runtime().starts_with("d1v5-engine@"), "{}", decider.command_runtime());
        let mut picks = vec![];
        let mut ms = vec![];
        for row in &rows {
            let r = parse(row["state"].as_str().unwrap())?;
            names::set(r.names.clone());
            let turns: Vec<_> = r.history.iter().enumerate().map(|(i, t)| text_turn(i as u64 + 1, t)).collect();
            let base = turns.len() as u64 + 1;
            let pending: Vec<_> = r.pending.iter().enumerate().map(|(i, t)| text_turn(base + i as u64, t)).collect();
            let snapshot = send::Snapshot { through: base + pending.len() as u64 - 1, turns, pending, working: r.working };
            let t = std::time::Instant::now();
            let (_, prediction) = decider.send_decision(&snapshot)?;
            ms.push(t.elapsed().as_secs_f64() * 1e3);
            picks.push(prediction.ack.clone().unwrap_or(prediction.choice.key().into()));
        }
        drop(decider);
        runs.push(json!({"precision": p.label(), "load_ms": load_ms, "decision_ms": ms, "picks": picks}));
    }
    let first = runs[0]["picks"].clone();
    anyhow::ensure!(runs[3]["picks"] == first, "4-bit after the swaps answered differently: {} vs {}", runs[3]["picks"], first);
    eprintln!("{}", serde_json::to_string_pretty(&json!({"rows": rows.len(), "runs": runs}))?);
    Ok(())
}
