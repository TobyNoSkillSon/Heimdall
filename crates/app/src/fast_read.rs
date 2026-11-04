//! `fast_read`: a message the agent wrote for the user to read word by word in the island (crates/ui `reader`, the
//! island's reader), and his reading-speed memory.
//!
//! The tool returns at once with a pending receipt; the island shows a ready pill until the user clicks it. When he
//! clicks "read it" (or closes it), the result reaches the agent as a notice through the action outbox, the same
//! durable path a confirmation answer takes: "Fast read "<title>": read at 265 wpm." or "…: dismissed".
//!
//! Speed memory (docs/research/rsvp-fast-reading.md, section 3): each read message is stored with its EmbeddingGemma 2
//! embedding (the timeline-search model, through the same shared service), the speed the user settled on and his
//! rewinds. The starting speed is the slow default until five messages exist, then the recency-weighted median of
//! his recent speeds; the topic term (nearest neighbours in embedding space) is used only once it has beaten that
//! median by at least 10 % on his own last 20 or more messages, replayed in order. The user's manual adjustment stays
//! primary: this only picks where the dial starts.

use agent::{actions::Actions, delivery::DeliveryPermit};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use ui::reader::{on_grid, FastRead, FastReadUpdate, ReadOutcome, ReadResult, DEFAULT_WPM};

/// One message the user read, as the speed memory keeps it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Record {
    pub unix_ms: u64,
    pub words: usize,
    pub start_wpm: u32,
    pub settled_wpm: u32,
    pub rewinds: u32,
    pub progress_pct: u8,
    /// Left in the first half: kept, never used for prediction.
    pub abandoned: bool,
    /// Difficulty features for a later residual model: mean letters per word, mean words per sentence.
    pub mean_word_len: f32,
    pub mean_sentence_len: f32,
    /// EmbeddingGemma 2 (768 floats) of the message; none when the embedder was unavailable.
    pub embedding: Option<Vec<f32>>,
}

/// Where a starting speed came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// Fewer than five messages: the slow default.
    Default,
    /// His recency-weighted median.
    Median,
    /// The median nudged toward the speeds of close neighbours in embedding space (the gate passed).
    Topic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prediction {
    pub wpm: u32,
    pub basis: Basis,
}

/// Messages before the personal median replaces the default.
const MIN_PERSONAL: usize = 5;
/// The median's window, and its recency half-life in messages.
const MEDIAN_WINDOW: usize = 20;
const MEDIAN_HALF_LIFE: f64 = 10.0;
/// Neighbour weighting: exp((cosine − 1) / TAU) × recency (half-life in days); the five nearest; shrink prior.
const TAU: f64 = 0.1;
const RECENCY_DAYS: f64 = 30.0;
const NEIGHBOURS: usize = 5;
const SHRINK_PRIOR: f64 = 2.0;
/// The gate: replay at least this many of his messages; the topic term must cut the median's error by 10 %.
const GATE_MIN: usize = 20;
const GATE_WINDOW: usize = 40;
const GATE_GAIN: f64 = 0.9;
/// Bounds the replay cost on a long history.
const HISTORY_CAP: usize = 500;

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += *x as f64 * *y as f64;
        na += *x as f64 * *x as f64;
        nb += *y as f64 * *y as f64;
    }
    if na == 0.0 || nb == 0.0 { 0.0 } else { dot / (na.sqrt() * nb.sqrt()) }
}

/// Recency-weighted median of the last 20 settled speeds, in log wpm (newest weighs most).
fn local(prior: &[&Record]) -> f64 {
    let recent: Vec<&&Record> = prior.iter().rev().take(MEDIAN_WINDOW).collect();
    let mut values: Vec<(f64, f64)> =
        recent.iter().enumerate().map(|(age, r)| ((r.settled_wpm as f64).ln(), 0.5f64.powf(age as f64 / MEDIAN_HALF_LIFE))).collect();
    values.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total: f64 = values.iter().map(|v| v.1).sum();
    let mut acc = 0.0;
    for (v, w) in &values {
        acc += w;
        if acc >= total / 2.0 {
            return *v;
        }
    }
    (DEFAULT_WPM as f64).ln()
}

/// The full scheme's estimate (log wpm): the median, plus the neighbours' deviation shrunk by how close they are.
fn topic(prior: &[&Record], embedding: &[f32], now_ms: u64) -> f64 {
    let base = local(prior);
    let mut weighted: Vec<(f64, f64)> = prior
        .iter()
        .filter_map(|r| {
            let e = r.embedding.as_deref()?;
            let days = now_ms.saturating_sub(r.unix_ms) as f64 / 86_400_000.0;
            let w = ((cosine(embedding, e) - 1.0) / TAU).exp() * 0.5f64.powf(days / RECENCY_DAYS);
            Some((w, (r.settled_wpm as f64).ln()))
        })
        .collect();
    weighted.sort_by(|a, b| b.0.total_cmp(&a.0));
    weighted.truncate(NEIGHBOURS);
    let sum: f64 = weighted.iter().map(|x| x.0).sum();
    if sum <= 0.0 {
        return base;
    }
    let topic = weighted.iter().map(|(w, v)| w * v).sum::<f64>() / sum;
    let shrink = sum / (sum + SHRINK_PRIOR);
    base + shrink * (topic - base)
}

/// Whether the topic term has earned its place: replaying his last messages in order (each predicted from the
/// ones before it), its mean absolute error in log wpm is at most 90 % of the median's, over 20 or more messages.
pub fn gate(eligible: &[&Record]) -> bool {
    let start = MIN_PERSONAL.max(eligible.len().saturating_sub(GATE_WINDOW));
    let (mut full, mut median, mut n) = (0.0, 0.0, 0usize);
    for i in start..eligible.len() {
        let Some(e) = eligible[i].embedding.as_deref() else { continue };
        let prior = &eligible[..i];
        let actual = (eligible[i].settled_wpm as f64).ln();
        full += (topic(prior, e, eligible[i].unix_ms) - actual).abs();
        median += (local(prior) - actual).abs();
        n += 1;
    }
    n >= GATE_MIN && full <= GATE_GAIN * median
}

/// The starting speed for a new message (its embedding when known), from his history (oldest first).
pub fn predict(history: &[Record], embedding: Option<&[f32]>, now_ms: u64) -> Prediction {
    let eligible: Vec<&Record> = history.iter().filter(|r| !r.abandoned).collect();
    let eligible = &eligible[eligible.len().saturating_sub(HISTORY_CAP)..];
    if eligible.len() < MIN_PERSONAL {
        return Prediction { wpm: DEFAULT_WPM, basis: Basis::Default };
    }
    if let Some(e) = embedding {
        if gate(eligible) {
            return Prediction { wpm: on_grid(topic(eligible, e, now_ms).exp()), basis: Basis::Topic };
        }
    }
    Prediction { wpm: on_grid(local(eligible).exp()), basis: Basis::Median }
}

/// Mean letters per word and words per sentence.
fn features(text: &str) -> (usize, f32, f32) {
    let words = ui::reader::words(text);
    let n = words.len().max(1);
    let letters: usize = words.iter().map(|w| w.letters).sum();
    let sentences = words.last().map_or(1, |w| w.sentence + 1);
    (words.len(), letters as f32 / n as f32, words.len() as f32 / sentences as f32)
}

/// The speed memory: one JSON line per message read, in the app's data directory.
pub struct Speeds {
    path: PathBuf,
    records: Mutex<Option<Vec<Record>>>,
}

impl Speeds {
    pub fn open(data: &Path) -> Self {
        Self { path: data.join("reading/speeds.jsonl"), records: Mutex::new(None) }
    }

    fn with<R>(&self, f: impl FnOnce(&mut Vec<Record>) -> R) -> R {
        let mut slot = self.records.lock().unwrap();
        let records = slot.get_or_insert_with(|| {
            // A line that does not parse (a torn final write) is skipped, never fatal.
            std::fs::read_to_string(&self.path).map(|t| t.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()).unwrap_or_default()
        });
        f(records)
    }

    #[cfg(test)]
    pub fn history(&self) -> Vec<Record> {
        self.with(|r| r.clone())
    }

    pub fn predict(&self, embedding: Option<&[f32]>) -> Prediction {
        self.with(|r| predict(r, embedding, agent::now_ms()))
    }

    pub fn append(&self, record: Record) -> Result<()> {
        self.with(|records| -> Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut line = serde_json::to_vec(&record)?;
            line.push(b'\n');
            std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?.write_all(&line)?;
            records.push(record);
            Ok(())
        })
    }
}

type Embed = Arc<dyn Fn(&str) -> Option<Vec<f32>> + Send + Sync>;

struct Pending {
    permit: DeliveryPermit,
    text: String,
    title: Option<String>,
    embedding: Option<Vec<f32>>,
}

/// Messages waiting for the user, the island handle and the speed memory.
pub struct Reads {
    pub speeds: Arc<Speeds>,
    pending: Arc<Mutex<HashMap<String, Pending>>>,
    ui: Mutex<Option<ui::Ui>>,
    /// Tests replace EmbeddingGemma 2 with fake vectors.
    embed: Mutex<Option<Embed>>,
}

impl Reads {
    pub fn new(data: &Path) -> Self {
        Self { speeds: Arc::new(Speeds::open(data)), pending: Arc::default(), ui: Mutex::new(None), embed: Mutex::new(None) }
    }

    /// The island is up (the agent runtime started with a UI).
    pub fn attach_ui(&self, ui: Option<ui::Ui>) {
        *self.ui.lock().unwrap() = ui;
    }

    #[cfg(test)]
    pub fn set_embedder(&self, embed: Embed) {
        *self.embed.lock().unwrap() = Some(embed);
    }

    fn embedder(&self, tools: &crate::bridge::Tools) -> Embed {
        if let Some(e) = self.embed.lock().unwrap().clone() {
            return e;
        }
        let service = tools.embeddings.clone();
        Arc::new(move |text: &str| {
            // The shared service (timeline search's model and worker); queries take priority over indexing.
            let mut cut = text.len().min(8192);
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let started = std::time::Instant::now();
            match service.query(&text[..cut], &mut || started.elapsed() < std::time::Duration::from_secs(30)) {
                Ok(v) => Some(v.values),
                Err(e) => {
                    eprintln!("fast_read: embedding unavailable: {e:#}");
                    None
                }
            }
        })
    }

    /// Agent off or the agent restarting: every waiting message leaves the island (its ready pill or open reader);
    /// no result is reported, the receipts stay as they were.
    pub fn withdraw_all(&self) {
        let ids: Vec<String> = self.pending.lock().unwrap().drain().map(|(id, _)| id).collect();
        if let Some(ui) = *self.ui.lock().unwrap() {
            for id in ids {
                ui.send(ui::UiUpdate::FastRead(FastReadUpdate::Withdraw(id)));
            }
        }
    }

    pub fn pending(&self, id: &str) -> bool {
        self.pending.lock().unwrap().contains_key(id)
    }
}

/// The `fast_read` tool: show the ready pill and return at once.
pub fn start(tools: &crate::bridge::Tools, actions: &Actions, args: &serde_json::Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
    let reads = &tools.fast_reads;
    let text = args["text"].as_str().context("INVALID_ARGUMENT: text required")?.to_string();
    let title = args["title"].as_str().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    anyhow::ensure!(!ui::reader::words(&text).is_empty(), "INVALID_ARGUMENT: text has no words");
    let ui = (*reads.ui.lock().unwrap()).context("UI_UNAVAILABLE: the island is not running; tell User another way")?;
    let id = actions.receipt(op, "fastread", &serde_json::to_string(&(&text, &title))?, None, permit)?;
    let state = actions.state(&id)?;
    if state == "pending" {
        let prediction = reads.speeds.predict(None);
        reads.pending.lock().unwrap().insert(id.clone(), Pending { permit: permit.clone(), text: text.clone(), title: title.clone(), embedding: None });
        permit.with(|| {
            ui.send(ui::UiUpdate::FastRead(FastReadUpdate::Show(FastRead { id: id.clone(), text: text.clone(), title: title.clone(), wpm: prediction.wpm })));
            Ok(())
        })?;
        actions.set_state(&id, "shown", permit)?;
        // The embedding (for the speed memory, and for the topic term once it has earned its place) is computed
        // off the tool call; a better starting speed reaches the pill if the user has not clicked yet.
        let embed = reads.embedder(tools);
        let (pending, speeds, id2) = (reads.pending.clone(), reads.speeds.clone(), id.clone());
        std::thread::Builder::new().name("fast-read-embed".into()).spawn(move || {
            let Some(vector) = embed(&text) else { return };
            let better = speeds.predict(Some(&vector));
            let mut pending = pending.lock().unwrap();
            let Some(p) = pending.get_mut(&id2) else { return };
            p.embedding = Some(vector);
            if better.basis == Basis::Topic && better.wpm != prediction.wpm && p.permit.check().is_ok() {
                ui.send(ui::UiUpdate::FastRead(FastReadUpdate::StartWpm { id: id2.clone(), wpm: better.wpm }));
            }
        })?;
    } else if state != "shown" {
        return Ok(format!("Fast read {id} already finished; its result was delivered as a notice."));
    }
    Ok(format!("Fast read pending: {id}. User clicks the island to start; his result (read at N wpm, or dismissed) arrives as a notice."))
}

/// What the agent hears back.
pub fn result_text(title: Option<&str>, r: &ReadResult) -> String {
    let name = title.map_or_else(|| "Fast read".to_string(), |t| format!("Fast read \"{t}\""));
    match r.outcome {
        ReadOutcome::Read => format!("{name}: read at {} wpm.", r.settled_wpm),
        ReadOutcome::Dismissed if r.progress_pct == 0 => format!("{name}: dismissed."),
        ReadOutcome::Dismissed => format!("{name}: dismissed after {}%.", r.progress_pct),
    }
}

/// The user read (or closed) a message: remember the speed and tell the agent. Off the main thread: the delivery waits
/// for the agent's durable acknowledgement.
pub fn finish(tools: Arc<crate::bridge::Tools>, result: ReadResult) {
    let spawned = std::thread::Builder::new().name("fast-read-result".into()).spawn(move || {
        if let Err(e) = finish_now(&tools, &result) {
            eprintln!("fast_read: result for {} not delivered: {e:#}", result.id);
        }
    });
    if let Err(e) = spawned {
        eprintln!("fast_read: could not start the result thread: {e}");
    }
}

pub(crate) fn finish_now(tools: &crate::bridge::Tools, result: &ReadResult) -> Result<()> {
    let reads = &tools.fast_reads;
    let Some(pending) = reads.pending.lock().unwrap().remove(&result.id) else {
        return Ok(());
    };
    // Speed: only from messages he actually read some of.
    if result.progress_pct > 0 {
        let embedding = pending.embedding.clone().or_else(|| reads.embedder(tools)(&pending.text));
        let (words, mean_word_len, mean_sentence_len) = features(&pending.text);
        reads.speeds.append(Record {
            unix_ms: agent::now_ms(),
            words,
            start_wpm: result.start_wpm,
            settled_wpm: result.settled_wpm,
            rewinds: result.rewinds,
            progress_pct: result.progress_pct,
            abandoned: result.progress_pct < 50,
            mean_word_len,
            mean_sentence_len,
            embedding,
        })?;
    }
    let actions = tools.mcp_actions.actions.as_ref().map_err(|e| anyhow::anyhow!(e.clone()))?;
    let permit = &pending.permit;
    // The agent that asked was switched off meanwhile: the speed is kept, nobody is told.
    permit.check()?;
    actions.set_state(&result.id, "completed", permit)?;
    actions.enqueue(&result.id, &result_text(pending.title.as_deref(), result), permit)?;
    actions.drain(permit)
}

/// After a restart the reader is gone, so a read still marked as shown can never finish: tell the agent it was
/// dismissed, once per read. Reads waiting in this process are left alone.
pub(crate) fn report_stale(tools: &crate::bridge::Tools, permit: &DeliveryPermit) -> Result<usize> {
    let actions = tools.mcp_actions.actions.as_ref().map_err(|e| anyhow::anyhow!(e.clone()))?;
    let mut reported = 0;
    for (id, body) in actions.receipts_in("fastread", "shown")? {
        if tools.fast_reads.pending(&id) { continue; }
        let title = serde_json::from_str::<(String, Option<String>)>(&body).ok().and_then(|(_, t)| t);
        let name = title.map_or_else(|| "Fast read".to_string(), |t| format!("Fast read \"{t}\""));
        // Outbox first (deduplicated by id), then the state: a crash in between repeats nothing.
        actions.enqueue(&id, &format!("{name}: dismissed (Heimdall restarted)."), permit)?;
        actions.set_state(&id, "completed", permit)?;
        reported += 1;
    }
    if reported > 0 { actions.drain(permit)?; }
    Ok(reported)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit vector near one of a few topic axes: `noise` spreads messages of one topic apart.
    fn fake(topic: usize, seed: u64, noise: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 768];
        v[topic * 7] = 1.0;
        let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        for x in v.iter_mut() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            *x += noise * (((s >> 33) as f32 / (1u64 << 31) as f32) - 0.5);
        }
        v
    }

    fn record(i: u64, wpm: u32, embedding: Option<Vec<f32>>) -> Record {
        Record { unix_ms: 1_760_000_000_000 + i * 3_600_000, words: 120, start_wpm: 240, settled_wpm: wpm, rewinds: 0, progress_pct: 100, abandoned: false, mean_word_len: 4.5, mean_sentence_len: 12.0, embedding }
    }

    #[test]
    fn agent_off_withdraws_every_waiting_read() {
        let dir = tempfile::tempdir().unwrap();
        let reads = Reads::new(dir.path());
        reads.attach_ui(Some(ui::Ui::handle()));
        let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
        for id in ["a", "b"] {
            reads.pending.lock().unwrap().insert(id.into(), Pending { permit: gate.permit().unwrap(), text: "x".into(), title: None, embedding: None });
        }
        reads.withdraw_all();
        assert!(!reads.pending("a") && !reads.pending("b"));
    }

    #[test]
    fn fewer_than_five_messages_keep_the_slow_default() {
        let history: Vec<_> = (0..4).map(|i| record(i, 340, Some(fake(0, i, 0.02)))).collect();
        assert_eq!(predict(&history, Some(&fake(0, 99, 0.02)), 0), Prediction { wpm: 240, basis: Basis::Default });
        assert_eq!(predict(&[], None, 0).wpm, 240);
    }

    #[test]
    fn five_messages_give_his_median_on_the_dial() {
        let history: Vec<_> = [300, 310, 290, 305, 600].iter().enumerate().map(|(i, w)| record(i as u64, *w, None)).collect();
        let p = predict(&history, None, 0);
        assert_eq!(p.basis, Basis::Median);
        // The recency-weighted median, rounded to a dial position (240 ± 25 steps).
        assert_eq!(p.wpm, 315);
        // Abandoned messages are kept but never counted.
        let mut h = history.clone();
        for r in h.iter_mut().take(2) {
            r.abandoned = true;
        }
        assert_eq!(predict(&h, None, 0).basis, Basis::Default);
    }

    /// Two familiar topics read at different speeds: once the replay shows the topic term beats the median, a new
    /// message lands near its topic's speed.
    #[test]
    fn topic_term_applies_once_it_beats_the_median_on_his_messages() {
        let mut history = vec![];
        for i in 0..40u64 {
            let (topic, wpm) = if i % 2 == 0 { (0, 340) } else { (1, 200) };
            history.push(record(i, wpm, Some(fake(topic, i, 0.02))));
        }
        let eligible: Vec<&Record> = history.iter().collect();
        assert!(gate(&eligible));
        let fast = predict(&history, Some(&fake(0, 1000, 0.02)), 1_760_200_000_000);
        let slow = predict(&history, Some(&fake(1, 1001, 0.02)), 1_760_200_000_000);
        assert_eq!(fast.basis, Basis::Topic);
        // Five close neighbours shrink at most 5/7 of the way from his median toward the topic's speed.
        assert!(fast.wpm >= 265 && fast.wpm <= 340, "{fast:?}");
        assert!(slow.wpm >= 190 && slow.wpm <= 215, "{slow:?}");
        assert!(fast.wpm >= slow.wpm + 75, "{fast:?} {slow:?}");
        // Without the new message's embedding, the median (between the two).
        assert_eq!(predict(&history, None, 0).basis, Basis::Median);
    }

    /// Speed unrelated to topic: the gate stays shut and the median is shown.
    #[test]
    fn unrelated_topics_keep_the_median() {
        let mut history = vec![];
        for i in 0..40u64 {
            let wpm = if (i * 7 + 3) % 5 < 2 { 290 } else { 265 };
            history.push(record(i, wpm, Some(fake((i % 4) as usize, i, 0.02))));
        }
        let eligible: Vec<&Record> = history.iter().collect();
        assert!(!gate(&eligible));
        assert_eq!(predict(&history, Some(&fake(0, 77, 0.02)), 0).basis, Basis::Median);
    }

    /// The gate needs 20 replayed messages.
    #[test]
    fn the_gate_needs_twenty_messages() {
        let history: Vec<_> = (0..26u64).map(|i| if i % 2 == 0 { record(i, 340, Some(fake(0, i, 0.02))) } else { record(i, 200, Some(fake(1, i, 0.02))) }).collect();
        let eligible: Vec<&Record> = history.iter().collect();
        // Replay starts after the first five: 24 messages replay 19, 25 replay 20.
        assert!(!gate(&eligible[..20]));
        assert!(!gate(&eligible[..24]));
        assert!(gate(&eligible[..25]));
        assert!(gate(&eligible[..26]));
    }

    #[test]
    fn speeds_persist_and_skip_a_torn_line() {
        let dir = tempfile::tempdir().unwrap();
        let speeds = Speeds::open(dir.path());
        speeds.append(record(1, 265, Some(fake(0, 1, 0.0)))).unwrap();
        speeds.append(record(2, 290, None)).unwrap();
        let path = dir.path().join("reading/speeds.jsonl");
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"unix_ms\":3,").unwrap();
        let reopened = Speeds::open(dir.path());
        let history = reopened.history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].embedding.as_ref().map(Vec::len), Some(768));
        assert_eq!(history[1].settled_wpm, 290);
    }

    #[test]
    fn result_lines_for_the_agent() {
        let r = |outcome, pct| ReadResult { id: "fastread-1".into(), outcome, start_wpm: 240, settled_wpm: 265, rewinds: 1, progress_pct: pct };
        assert_eq!(result_text(Some("Build failure"), &r(ReadOutcome::Read, 100)), "Fast read \"Build failure\": read at 265 wpm.");
        assert_eq!(result_text(None, &r(ReadOutcome::Dismissed, 0)), "Fast read: dismissed.");
        assert_eq!(result_text(None, &r(ReadOutcome::Dismissed, 30)), "Fast read: dismissed after 30%.");
    }
}
