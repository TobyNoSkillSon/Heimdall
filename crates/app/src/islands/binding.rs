//! Thread-bound islands (night 1022): `islands_put` with `thread` binds a job island to a T3 thread; after every T3
//! pass (`t3::Workers::passes`) each bound island is brought in line with its thread: state, stage, attention and
//! the thread's open question. The question is answered on the notch's card (`ui::UiUpdate::AgentNotice`, id
//! `t3:<island>:<request>`, every question of the request, 24 Oct), and the answer goes back through the T3 client.
//! When the thread settles (completed or failed, no open question) the island shows done or failed and stays bound:
//! a steer or a follow-up starts a new run on the same thread, and the island follows it live (
//! before, it unbound and latched "done" while the thread worked again). An island whose file names a thread is
//! (re)bound at every pass, so islands written before this, or by hand, follow their thread too.
//! Bindings persist in `<Heimdall data>/islands-bindings.json`.
//!
//! Code-driven islands (30 Oct, background-jobs redesign): the live source also reads each bound thread's new activity
//! items (only when its item count moved), its sub-agents and the TrainingLab run its commands point at, into one
//! `stages::Reducer` per run kept in memory; `sync_patch` then writes, within the pass, what code owns on a bound
//! island: the stage, the clock (`started`), the compact history (`steps`), counters (`kv`), the agent's latest
//! sentence (`summary`), `progress` where it is measurable and the latest picture (copied into the islands folder,
//! `pictures/`, since pictures in /tmp vanish). A code value is written when it changed since code last wrote it, so
//! another writer's value stands until code has something new; `parts` stay the island agent's and the main agent's.
//! After each sync the island's signals go to the island agent (`island_agent::Snapshot`), which adds the graphic.

use super::files::Root;
use super::stages::{self, Shown};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A thread's open request: every question of it, in order (T3's `questions`), as the notch card shows them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub request: String,
    #[serde(default)]
    pub items: Vec<Item>,
}

/// One question of a request. Its `id` keys the answer (T3's `answers` map).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub header: Option<String>,
    pub text: String,
    pub multi_select: bool,
    /// T3's `allowCustomAnswer`: the card's Other row.
    pub allow_other: bool,
    pub options: Vec<Opt>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Opt {
    pub label: String,
    pub description: Option<String>,
}

/// The user's answer to one question of a request: the labels he picked and, when the card allows it, his own words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pick {
    pub question_id: String,
    pub labels: Vec<String>,
    pub other: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub thread: String,
    /// The request shown last, so an answer can be checked against it.
    #[serde(default)]
    pub question: Option<Question>,
    /// The thread's run as last synced: a new one means the stage on the island is the last run's (bug 2026-10-28T18-55-56).
    #[serde(default)]
    pub run: Option<String>,
}

/// Where a bound thread's open request is shown: the notch card (`UiUpdate::AgentNotice`, `DismissAgentNotice`).
pub type CardSink = Arc<dyn Fn(ui::UiUpdate) + Send + Sync>;

/// The notch card's id for a request on an island.
pub fn card_id(island: &str, request: &str) -> String {
    format!("t3:{island}:{request}")
}

/// The island and request a card id names (`card_id`), when the id is a T3 card's.
pub fn parse_card_id(id: &str) -> Option<(String, String)> {
    let rest = id.strip_prefix("t3:")?;
    let (island, request) = rest.split_once(':')?;
    Some((island.to_owned(), request.to_owned()))
}

/// The notch card for a request: one tab per question, answered together with Send.
pub fn card(island: &str, q: &Question) -> ui::agent_notice::AgentNotice {
    use ui::agent_notice::{AgentNotice, AskOption, AskQuestion, NoticeAction, NoticeKind};
    let first = q.items.first().map(|i| i.text.clone()).unwrap_or_default();
    let mut notice = AgentNotice::line(card_id(island, &q.request), first, NoticeKind::Info);
    let questions = q.items.iter().map(|i| AskQuestion {
        id: i.id.clone(),
        header: i.header.clone(),
        text: i.text.clone(),
        multi_select: i.multi_select,
        allow_other: i.allow_other, field: None,
        options: i.options.iter().map(|o| AskOption { label: o.label.clone(), description: o.description.clone(), grant: vec![] }).collect(),
    }).collect();
    // T3 keeps its request until it is answered or the thread ends; the card's own clock is a day, as an `ask` is.
    let expires_ms = chrono::Utc::now().timestamp_millis().max(0) as u64 + 24 * 60 * 60 * 1000;
    notice.action = Some(NoticeAction::Ask { request_id: card_id(island, &q.request), questions, expires_ms, countdown: false });
    notice
}

/// T3's `answers` for a request: each question's id maps to its answer. A single-select question takes one string
/// (his label, or his own words); a multi-select question takes an array of labels, with his own words last.
/// UNVERIFIED against T3 (24 Oct): the multi-select shape is ours until a live request confirms it.
/// A question the user left without a pick when he sent the card (bug note 2026-11-02T16-27-21) goes as the words
/// "(unanswered)", since T3 0.1.6 refuses a respond that leaves a question out ("Answer each question before
/// sending."); a question that takes no words of his own cannot be left, and nothing answered at all is refused.
pub fn t3_answers(q: &Question, picks: &[Pick]) -> Result<Map<String, Value>> {
    let mut out = Map::new();
    let mut answered = false;
    for item in &q.items {
        let given = picks.iter().find(|p| p.question_id == item.id)
            .filter(|p| !p.labels.is_empty() || p.other.as_deref().is_some_and(|t| !t.trim().is_empty()));
        let Some(pick) = given else {
            anyhow::ensure!(item.allow_other, "answer '{}': it takes one of its options", item.text);
            let left = agent::actions::UNANSWERED;
            out.insert(item.id.clone(), if item.multi_select { json!([left]) } else { json!(left) });
            continue;
        };
        answered = true;
        if let Some(bad) = pick.labels.iter().find(|l| !item.options.iter().any(|o| &o.label == *l)) {
            anyhow::bail!("'{bad}' is not an option of '{}'", item.text);
        }
        let other = pick.other.as_deref().map(str::trim).filter(|t| !t.is_empty());
        anyhow::ensure!(other.is_none() || item.allow_other, "'{}' takes no answer of its own", item.text);
        let value = if item.multi_select {
            anyhow::ensure!(!pick.labels.is_empty() || other.is_some(), "pick at least one answer for '{}'", item.text);
            let mut list: Vec<Value> = pick.labels.iter().map(|l| json!(l)).collect();
            list.extend(other.map(|t| json!(t)));
            Value::Array(list)
        } else {
            match (pick.labels.as_slice(), other) {
                ([label], None) => json!(label),
                ([], Some(text)) => json!(text),
                _ => anyhow::bail!("answer '{}' with one option or your own words, not both", item.text),
            }
        };
        out.insert(item.id.clone(), value);
    }
    anyhow::ensure!(answered, "nothing answered");
    Ok(out)
}

/// Island id → binding, saved on every change.
pub struct Bindings {
    path: Option<PathBuf>,
    map: Mutex<BTreeMap<String, Binding>>,
    /// Shows and hides a bound thread's open request on the notch (`set_card_sink`).
    cards: Mutex<Option<CardSink>>,
    /// Requests the user answered, by card id, and when: T3 may keep listing one for a while after the respond, and a sync
    /// must not show it again (R6).
    answered: Mutex<BTreeMap<String, std::time::Instant>>,
    /// What code last wrote per island (`CODE_KEYS`), in memory: a key is written again only when code's value changed.
    wrote: Mutex<BTreeMap<String, Map<String, Value>>>,
    /// Each island's latest picture: (the file the thread named, its copy in `pictures/`).
    pictures: Mutex<BTreeMap<String, (String, String)>>,
    /// Where each bound island's signals go after a sync (the island agent).
    agent: Mutex<Option<tokio::sync::mpsc::UnboundedSender<crate::island_agent::Snapshot>>>,
}

/// The island keys code owns on a bound island while it has signals.
pub const CODE_KEYS: [&str; 4] = ["stage", "started", "summary", "progress"];

/// How long an answered request stays out of the notch after its respond (R6).
pub const ANSWERED_MEMORY: std::time::Duration = std::time::Duration::from_secs(5 * 60);

impl Bindings {
    pub fn load(path: Option<PathBuf>) -> Bindings {
        let map = path.as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Bindings { path, map: Mutex::new(map), cards: Mutex::new(None), answered: Mutex::new(BTreeMap::new()),
                   wrote: Mutex::default(), pictures: Mutex::default(), agent: Mutex::new(None) }
    }
    /// Where bound islands' signals go after each sync (`island_agent::spawn_live`).
    pub fn set_agent(&self, tx: Option<tokio::sync::mpsc::UnboundedSender<crate::island_agent::Snapshot>>) {
        *self.agent.lock().unwrap_or_else(|e| e.into_inner()) = tx;
    }
    fn to_agent(&self, snap: crate::island_agent::Snapshot) {
        if let Some(tx) = self.agent.lock().unwrap_or_else(|e| e.into_inner()).as_ref() { let _ = tx.send(snap); }
    }
    /// Code's keys in `patch` that code wrote last time with the same value go (another writer's value stands until
    /// code has something new), and so does a clear of a key code never set. Remembers what is left.
    fn only_new(&self, island: &str, patch: &mut Map<String, Value>) {
        let mut wrote = self.wrote.lock().unwrap_or_else(|e| e.into_inner());
        let last = wrote.entry(island.to_owned()).or_default();
        let taking_back = patch.get("summaryRun").is_some_and(Value::is_null);
        patch.retain(|k, v| {
            if !CODE_KEYS.contains(&k.as_str()) { return true }
            // Code takes the now line back from a stale agent line: written even if code wrote the same line before.
            if k == "summary" && taking_back { last.insert(k.clone(), v.clone()); return true }
            if last.get(k) == Some(v) { return false }
            if v.is_null() && last.get(k).is_none_or(Value::is_null) { return false }
            last.insert(k.clone(), v.clone());
            true
        });
    }
    /// Remembers that the user answered `request` on `island`: until `ANSWERED_MEMORY` passes, T3 still listing it is not a
    /// new question (R6).
    pub fn remember_answered(&self, island: &str, request: &str) {
        let mut answered = self.answered.lock().unwrap_or_else(|e| e.into_inner());
        answered.retain(|_, at| at.elapsed() < ANSWERED_MEMORY);
        answered.insert(card_id(island, request), std::time::Instant::now());
    }
    pub fn recently_answered(&self, island: &str, request: &str) -> bool {
        self.answered.lock().unwrap_or_else(|e| e.into_inner()).get(&card_id(island, request)).is_some_and(|at| at.elapsed() < ANSWERED_MEMORY)
    }
    /// Forgets every island's open request without touching the notch (the cards are cleared with the agent's
    /// questions): the next sync shows each one again (R1).
    pub fn forget_questions(&self) {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        for b in map.values_mut() { changed |= b.question.take().is_some(); }
        if changed { self.save(&map); }
    }
    /// Where open requests go (the app's notch). Until it is set, requests are only kept in the binding.
    pub fn set_card_sink(&self, sink: CardSink) {
        *self.cards.lock().unwrap_or_else(|e| e.into_inner()) = Some(sink);
    }
    fn save(&self, map: &BTreeMap<String, Binding>) {
        let Some(p) = &self.path else { return };
        if let Err(e) = super::files::write_atomic(p, &serde_json::to_value(map).unwrap_or_default()) {
            eprintln!("islands: bindings not saved: {e:#}");
        }
    }
    fn edit<T>(&self, f: impl FnOnce(&mut BTreeMap<String, Binding>) -> T) -> T {
        let mut map = self.map.lock().unwrap_or_else(|e| e.into_inner());
        let out = f(&mut map);
        self.save(&map);
        out
    }
    pub fn bind(&self, island: &str, thread: &str) {
        self.edit(|m| {
            if m.get(island).is_none_or(|b| b.thread != thread) {
                m.insert(island.into(), Binding { thread: thread.into(), question: None, run: None });
            }
        })
    }
    pub fn unbind(&self, island: &str) -> bool {
        let removed = self.edit(|m| m.remove(island));
        self.wrote.lock().unwrap_or_else(|e| e.into_inner()).remove(island);
        self.pictures.lock().unwrap_or_else(|e| e.into_inner()).remove(island);
        let Some(binding) = removed else { return false };
        // A card for the request the island no longer follows goes with it.
        if let Some(old) = binding.question { self.show(|s| s(ui::UiUpdate::DismissAgentNotice(card_id(island, &old.request)))); }
        true
    }
    fn show(&self, f: impl FnOnce(&CardSink)) {
        let sink = self.cards.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(sink) = sink { f(&sink) }
    }
    /// Records the thread's run as synced.
    pub fn set_run(&self, island: &str, run: &str) {
        self.edit(|m| if let Some(b) = m.get_mut(island) { b.run = Some(run.into()) })
    }
    pub fn get(&self, island: &str) -> Option<Binding> { self.map.lock().unwrap_or_else(|e| e.into_inner()).get(island).cloned() }
    pub fn all(&self) -> Vec<(String, Binding)> {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }
    /// Sets the island's open request and shows its card when it is new; the card goes when the request does.
    pub fn set_question(&self, island: &str, q: Option<Question>) {
        let change = self.edit(|m| {
            let b = m.get_mut(island)?;
            if b.question == q { return None }
            Some(std::mem::replace(&mut b.question, q.clone()))
        });
        let Some(before) = change else { return };
        if let Some(old) = before.filter(|old| q.as_ref().is_none_or(|n| n.request != old.request)) {
            self.show(|s| s(ui::UiUpdate::DismissAgentNotice(card_id(island, &old.request))));
        }
        if let Some(q) = q.filter(|q| !q.items.is_empty()) {
            self.show(|s| s(ui::UiUpdate::AgentNotice(card(island, &q))));
        }
    }
}

/// What the island needs to know about its thread.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadView {
    pub status: String,
    pub title: String,
    pub pending: u64,
    pub question: Option<Question>,
    /// Sub-agents still at work (read only once the thread's own turn completed): the job is not done yet.
    pub working_children: u64,
    /// Sub-agents not settled that changed within the hour: they may be waiting on background work (a GPU lock, a
    /// long render) without changing, so the thread is not settled yet; the plan refresher, which reads them, decides.
    pub recent_children: u64,
    /// The thread's own turn ended within the last hour: what the whole job (sub-agents, background work) is doing
    /// is the plan refresher's to read, so the binding does not declare it done yet.
    pub ended_recently: bool,
    /// Background commands and monitors still running by Heimdall's own record of the task (`t3::Workers::background_work`,
    /// across all its runs): a turn that ended with them is not done (bug note 2026-11-03T22-49-42).
    pub background: u64,
    /// The thread was archived (`task_cancel`, or by hand): nothing more comes of it unless a new run starts, so its
    /// island ends now instead of waiting out the hour (bugs 23 Oct 22:12 and 23:05).
    pub archived: bool,
    /// The thread's latest run (T3's `latestRunId`).
    pub run: String,
    /// `run` is not the one the island last synced to: a send started a new run (`sync_one`).
    pub new_run: bool,
    /// What code reads of the run (`stages`): `None` when the items could not be read this pass (the island then
    /// changes only with the thread's state, as before).
    pub signals: Option<stages::Signals>,
    /// The thread as last read, for the island agent's activity digest.
    pub raw: Option<super::digest::ThreadRaw>,
    /// The approval this run waits on (`t3::Workers::approval_wait`): the card's text (the command and Claude's
    /// reason) and the link that opens the thread in T3. T3 itself still says "running" while a prompt waits.
    pub approval: Option<Approval>,
}

/// An approval prompt the run waits on, as the island shows it (`ThreadView::approval`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Approval {
    pub text: Option<String>,
    pub link: Option<String>,
}

/// The run a thread is on, its state and its live request count, from one `t3_thread_read` answer. A run T3 queued and
/// cancelled before it started is not the thread's state (`t3::threads::current_run`, bug note 2026-11-04T22-25-15):
/// the task island stays on the run that works instead of reading "Stopped". Returns the current run's entry too.
pub fn current_state(r: &Value) -> (String, String, u64, Option<&Value>) {
    let t = &r["thread"];
    let runs = r["recentRuns"].as_array().map(Vec::as_slice).unwrap_or_default();
    let (run, status) = t3::threads::current_run(t, runs);
    let run = run.unwrap_or_default();
    // A finished run's leftover count (an approval the hook answered) is no wait (`t3::threads::live_requests`); the
    // state is the current run's, not the dropped one's.
    let pending = if ACTIVE.contains(&status.as_str()) { t["pendingRequestCount"].as_u64().unwrap_or(0) } else { 0 };
    let entry = runs.iter().find(|x| x["runId"].as_str() == Some(run.as_str())).or_else(|| runs.first());
    (run, status, pending, entry)
}

/// The approval to show for `view`: the wait Heimdall's T3 pass recorded for this very run, while T3 still counts a
/// live request on it (a wait the pass has not cleared yet is not shown once the prompt is gone).
pub fn approval_for(view: &ThreadView, wait: Option<t3::ApprovalWait>) -> Option<Approval> {
    let w = wait?;
    (view.pending > 0 && !view.run.is_empty() && w.run == view.run && view.question.is_none())
        .then(|| Approval { text: w.text.filter(|t| !t.trim().is_empty()), link: w.link.filter(|l| !l.trim().is_empty()) })
}

/// How recently a sub-agent must have changed to count as still working. T3 lists native sub-agents as `idle`
/// however busy they are; only their `updatedAt` moves while they work.
pub const CHILD_QUIET_S: i64 = 4 * 60;
/// Threads listed per sub-agent lookup (newest first; T3 has no parent filter, and a project can be busy).
pub const CHILD_LIST_LIMIT: u64 = 100;

/// The sub-agents of `thread` in a `t3_thread_list` answer (`includeSubagents`), newest first.
pub fn children<'a>(list: &'a Value, thread: &str) -> Vec<&'a Value> {
    list["threads"].as_array().into_iter().flatten().filter(|t| t["parentThreadId"].as_str() == Some(thread)).collect()
}

/// A sub-agent not settled that changed within the last hour.
pub fn recent_child(child: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    child["settled"].as_bool() != Some(true)
        && child["updatedAt"].as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| (now - t.with_timezone(&chrono::Utc)).num_minutes() < 60)
}

/// A sub-agent still at work at `now`: running or asking, or not settled and changed within `CHILD_QUIET_S`.
pub fn child_working(child: &Value, now: chrono::DateTime<chrono::Utc>) -> bool {
    let status = child["status"].as_str().unwrap_or("");
    if ACTIVE.contains(&status) || t3::threads::asks(child) { return true }
    if child["settled"].as_bool() == Some(true) || !matches!(status, "idle" | "") { return false }
    child["updatedAt"].as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| (now - t.with_timezone(&chrono::Utc)).num_seconds() < CHILD_QUIET_S)
}

const ACTIVE: [&str; 4] = ["preparing", "queued", "starting", "running"];
/// Stages Heimdall writes itself; anything else is the agent's and survives while the thread runs.
const OWN_STAGES: [&str; 5] = ["Your answer", "Needs approval", "Done", "Failed", "Stopped"];

/// The moment an island agent's now line belongs to: the compact stage while running, the question, or the end.
pub fn summary_key<'a>(shown: Shown, stage: &'a str) -> &'a str {
    match shown { Shown::Running => stages::key_stage(stage), Shown::Asking => "Your answer", Shown::Ended => "end" }
}

/// A thread whose turn ended counts as quiet (its sub-agents are not listed again for ten minutes) only when none of
/// them works now and none changed within the hour. Review, 30 Oct: working sub-agents are not in `recent`, so a
/// thread whose children all worked was marked quiet, and the next pass declared it done while they ran.
pub fn settles_quiet(working: u64, recent: u64) -> bool { working == 0 && recent == 0 }

/// Code's band part (`stages::Signals::band`).
pub fn is_band(part: &Value) -> bool { part["type"] == "band" && part["code"] == true }

fn ours(part: &Value) -> bool {
    part.get("t3q").and_then(Value::as_bool) == Some(true)
        || part.get("action").and_then(Value::as_str).is_some_and(|a| a.starts_with("t3q:"))
}

/// The island's parts without Heimdall's question parts, plus `extra`.
fn parts_with(island: &Map<String, Value>, extra: Vec<Value>) -> Value {
    let mut parts: Vec<Value> = island.get("parts").and_then(Value::as_array).into_iter().flatten().filter(|p| !ours(p)).cloned().collect();
    parts.extend(extra);
    if parts.is_empty() { Value::Null } else { Value::Array(parts) }
}

/// The floating island says what is asked; the answer is on the notch's card (no buttons here, 24 Oct).
fn question_parts(q: &Question) -> Vec<Value> {
    let first = q.items.first().map(|i| i.text.as_str()).unwrap_or_default();
    let more = q.items.len().saturating_sub(1);
    let value = if more == 0 { first.to_owned() } else { format!("{first} (+{more} more)") };
    vec![json!({"type": "text", "value": value, "t3q": true})]
}

/// The approval the run waits on: what it asks to run (the command and Claude's reason), and the button that opens
/// the thread in T3, where only the user can give it. Heimdall's parts, gone once the wait ends.
fn approval_parts(a: &Approval) -> Vec<Value> {
    let mut parts = vec![];
    if let Some(t) = &a.text { parts.push(json!({"type": "text", "value": t, "t3q": true})) }
    if let Some(l) = &a.link { parts.push(json!({"type": "openButton", "target": l, "t3q": true})) }
    parts
}

/// The island change for a thread's state (`null` removes a key), and whether the thread has settled.
pub fn sync_patch(island: &Map<String, Value>, view: &ThreadView) -> (Map<String, Value>, bool) {
    let stage_now = island.get("stage").and_then(Value::as_str);
    // A new run's work is not the last run's: its stage starts over until the plan refresher names it (bug note
    // 2026-10-28T18-55-56: a stage written for one run stayed while the task moved on).
    let fresh = view.new_run && ACTIVE.contains(&view.status.as_str());
    let keep_stage = |fallback: &str| match stage_now {
        Some(s) if !fresh && !OWN_STAGES.contains(&s) => json!(s),
        _ => json!(fallback),
    };
    let s = view.status.as_str();
    // The run ended on something the plan refresher marked (waiting on the user's pick, a failure it read): kept below.
    let marked = matches!(island.get("state").and_then(Value::as_str), Some("waiting" | "failed")) && island.get("attention").is_none_or(Value::is_null);
    // Heimdall's record of the task's background commands and monitors, across all its runs (bug note
    // 2026-11-03T22-49-42: the latest run's items showed nothing open while a restore and its monitors ran).
    let background = s == "completed" && view.background > 0 && !marked;
    let with_record = view.signals.clone().map(|mut g| {
        if background { g.background_open = g.background_open.max(view.background as usize) }
        g
    });
    let sig = with_record.as_ref();
    // With signals, code names the stage (the pill) while the job runs.
    let running_stage = |fallback: &str| sig.map_or_else(|| keep_stage(fallback), |g| json!(g.pill()));
    let (state, stage, attention, extra, settled) = if view.archived && !ACTIVE.contains(&s) {
        // Archived and not running: the job is over now, sub-agents, the hour and any open question with it. A failure
        // the refresher read stays; anything else ends done, failed or stopped.
        match s {
            "failed" | "rolled_back" => ("failed", json!("Failed"), Value::Null, vec![], true),
            "completed" if island.get("state").and_then(Value::as_str) == Some("failed") && island.get("attention").is_none_or(Value::is_null) => {
                return (Map::new(), true)
            }
            "completed" => ("done", json!("Done"), Value::Null, vec![], true),
            _ => ("failed", json!("Stopped"), Value::Null, vec![], true),
        }
    } else if let Some(q) = view.question.as_ref().filter(|_| view.pending > 0 || s == "waiting") {
        ("waiting", json!("Your answer"), json!("question"), question_parts(q), false)
    } else if let Some(a) = view.approval.as_ref() {
        // T3 says "running" while an approval prompt holds the run (bug note 2026-11-04T18-30-26).
        ("waiting", json!("Needs approval"), json!("waiting"), approval_parts(a), false)
    } else if s == "waiting" || (view.pending > 0 && !ACTIVE.contains(&s)) {
        ("waiting", json!("Needs approval"), json!("waiting"), vec![], false)
    } else if ACTIVE.contains(&s) {
        ("running", running_stage("Working"), Value::Null, vec![], false)
    } else if s == "completed" && view.working_children > 0 {
        // Its own turn ended, its sub-agents still work: not done, and not settled.
        ("running", running_stage("Working"), Value::Null, vec![], false)
    } else if sig.is_none() && background {
        // Without signals, the same record (`background`): "the island says it's finished, but it's not" (bug note
        // 2026-11-03T22-49-42). Running, not settled, until they end.
        ("running", json!("Background"), Value::Null, vec![], false)
    } else if let Some(g) = sig.filter(|g| s == "completed" && (view.recent_children > 0 || g.background_open > 0)) {
        // Code read the run: the turn ended, but sub-agents changed within the hour (one may wait on a GPU lock) or a
        // background command it started has not reported back. Still the job, running; an hour of quiet settles it.
        let stage = if view.recent_children > 0 { "Sub-agents" } else { "Background" };
        let _ = g;
        ("running", json!(stage), Value::Null, vec![], false)
    } else if sig.is_none() && s == "completed" && (view.recent_children > 0 || view.ended_recently) {
        // The turn ended within the hour, or sub-agents changed within it: the job may go on in sub-agents or
        // background work. Leave state and stage to the plan refresher, which reads all of it; only a question that
        // was answered goes away. An hour of quiet settles below.
        let mut patch = Map::new();
        patch.insert("attention".into(), Value::Null);
        patch.insert("parts".into(), parts_with(island, vec![]));
        // A question or approval of ours that was answered: no longer waiting on the user (the refresher may say more).
        if island.get("attention").is_some_and(|a| !a.is_null()) {
            patch.insert("state".into(), json!("running"));
            patch.insert("stage".into(), json!("Working"));
        }
        patch.retain(|k, v| island.get(k).unwrap_or(&Value::Null) != v);
        return (patch, false);
    } else if s == "completed" && matches!(island.get("state").and_then(Value::as_str), Some("waiting" | "failed")) && island.get("attention").is_none_or(Value::is_null) {
        // The run ended on something T3 cannot say: waiting on the user ("Your pick") or a failure it reported in words.
        // The plan refresher marked it; keep that until a new run starts (no flipping back and forth).
        return (Map::new(), true);
    } else if s == "completed" {
        ("done", json!("Done"), Value::Null, vec![], true)
    } else if matches!(s, "failed" | "rolled_back") {
        ("failed", json!("Failed"), Value::Null, vec![], true)
    } else if matches!(s, "interrupted" | "cancelled") {
        ("failed", json!("Stopped"), Value::Null, vec![], true)
    } else {
        // Idle or unknown: nothing to say yet.
        return (Map::new(), false);
    };
    let mut patch = Map::new();
    patch.insert("state".into(), json!(state));
    patch.insert("stage".into(), stage);
    patch.insert("attention".into(), attention);
    patch.insert("parts".into(), parts_with(island, extra));
    match sig {
        // Code owns the stage, clock, history, counters, line and progress of a bound island it can read.
        Some(g) => {
            let shown = match state { "done" | "failed" => Shown::Ended, "waiting" => Shown::Asking, _ => Shown::Running };
            // The island agent's now line stands only while it was written for this run and this moment (the compact
            // stage, the question, or the end); otherwise code takes the line back (review: with Islands off or no
            // quota, a done island kept "Downloading the 9 GB voice weights").
            let line = island.get("summaryRun").and_then(Value::as_str) == Some(g.run.as_str())
                && island.get("summaryStage").and_then(Value::as_str) == Some(summary_key(shown, &g.stage));
            let ours = g.run.is_empty() || !line;
            for (k, v) in g.island_keys(shown, ours) { patch.insert(k, v); }
            if ours && island.contains_key("summaryRun") {
                patch.insert("summaryRun".into(), Value::Null);
                patch.insert("summaryStage".into(), Value::Null);
            }
            // The band (round 3) is the last part: helper rows and the stage ribbon, drawn from facts.
            let mut parts: Vec<Value> = patch.get("parts").and_then(Value::as_array).cloned().unwrap_or_default();
            parts.retain(|p| !is_band(p));
            parts.push(g.band(shown, state, island.get("helpers")));
            patch.insert("parts".into(), Value::Array(parts));
        }
        // Without signals the stage is written only on a change of state, as before (another writer named it).
        None => if island.get("state").and_then(Value::as_str) == Some(state) && !fresh { patch.remove("stage"); },
    }
    // Only what differs, so an unchanged thread never rewrites the file.
    patch.retain(|k, v| island.get(k).unwrap_or(&Value::Null) != v);
    (patch, settled)
}

/// The island's latest picture: the newest of `named` that is a real image file, copied into `<root>/pictures/` as
/// `<island>-<hash>.<ext>` (the island's older copies go). Copied once per picture; `None` when none is real.
pub fn keep_picture(root: &Root, bindings: &Bindings, island: &str, named: &[String]) -> Option<String> {
    let src = named.iter().find(|p| super::picture(p))?;
    if let Some((from, copy)) = bindings.pictures.lock().unwrap_or_else(|e| e.into_inner()).get(island) {
        if from == src && std::path::Path::new(copy).is_file() { return Some(copy.clone()) }
    }
    let dir = root.0.join("pictures");
    std::fs::create_dir_all(&dir).ok()?;
    let ext = std::path::Path::new(src).extension().and_then(|e| e.to_str()).unwrap_or("png").to_ascii_lowercase();
    let stem = super::files::safe_id(island);
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        src.hash(&mut h);
        std::fs::metadata(src).ok().and_then(|m| m.modified().ok()).hash(&mut h);
        format!("{:08x}", h.finish() as u32)
    };
    let copy = dir.join(format!("{stem}-{hash}.{ext}"));
    if !copy.is_file() {
        let tmp = dir.join(format!(".{stem}-{hash}.tmp"));
        std::fs::copy(src, &tmp).ok()?;
        std::fs::rename(&tmp, &copy).ok()?;
    }
    // One picture per island: the older copies go.
    for e in std::fs::read_dir(&dir).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with(&format!("{stem}-")) && e.path() != copy { let _ = std::fs::remove_file(e.path()); }
    }
    let copy = copy.to_string_lossy().into_owned();
    bindings.pictures.lock().unwrap_or_else(|e| e.into_inner()).insert(island.to_owned(), (src.clone(), copy.clone()));
    Some(copy)
}

/// Where thread state comes from and answers go: T3's client in the app, a fake in tests.
#[async_trait::async_trait]
pub trait ThreadSource: Send + Sync {
    async fn view(&self, thread: &str) -> Result<ThreadView>;
    /// Sends the user's answers for every question of `q` (`t3_answers`).
    async fn answer(&self, thread: &str, q: &Question, picks: &[Pick]) -> Result<()>;
    /// The user is done with `thread`'s island: archive the task once it is finished and reported (`Workers::clear_task`).
    /// Returns the titles archived now.
    async fn cleared(&self, _thread: &str) -> Result<Vec<String>> { Ok(vec![]) }
}

/// The live source: the same `t3::Client` calls the agent's T3 tools make. A finished thread is followed forever
/// (a steer can start a new run), so its sub-agents are listed again only when the thread moved, when some were
/// recent at the last look, or every 10 minutes.
pub struct T3Source {
    pub workers: Arc<t3::Workers>,
    quiet: Mutex<std::collections::HashMap<String, (u64, std::time::Instant)>>,
    /// Each bound thread's reducer and what was last read of it, in memory (a restart re-reads the latest run's tail).
    tracks: Mutex<std::collections::HashMap<String, Track>>,
    /// TrainingLab's runs folder (`train::runs_root`; none in the public edition).
    runs: Option<std::path::PathBuf>,
}

impl T3Source {
    pub fn new(workers: Arc<t3::Workers>) -> T3Source {
        T3Source { workers, quiet: Mutex::default(), tracks: Mutex::default(), runs: super::train::runs_root() }
    }
}

/// Activity items read per page, pages per pass, and the tail a new run (or a restart) starts from.
pub const PAGE_ITEMS: u64 = 100;
pub const PAGES: usize = 3;
pub const TAIL_ITEMS: u64 = 200;
/// Characters per item (a command's head; a test command's output tail is read on its own, `TEST_TAIL`).
pub const ITEM_CHARS: u64 = 1200;
pub const TEST_TAIL: usize = 600;
/// Items kept per thread for the island agent's digest.
pub const RECENT_ITEMS: usize = 40;
/// While a thread runs, its sub-agents are listed again after this long even when it did not move.
pub const LIST_EVERY: std::time::Duration = std::time::Duration::from_secs(120);

/// One bound thread as code follows it.
struct Track {
    reducer: stages::Reducer,
    count: u64,
    recent: std::collections::VecDeque<Value>,
    children: Vec<Value>,
    listed: Option<std::time::Instant>,
    train: Option<std::path::PathBuf>,
    /// Test commands still running when read: read again by id until they finish (their pass counts).
    tests: Vec<String>,
}

impl T3Source {
    /// The thread's activity items after the last one read (a new run, or a restart: the latest `TAIL_ITEMS`), at
    /// most `PAGES` pages, into the reducer; a finished test command's output tail is read on its own (its pass
    /// counts sit at the end). Returns whether the read answered.
    async fn read_items(&self, thread: &str, count: u64, tr: &mut Track) -> bool {
        let c = &self.workers.client;
        // Test commands that were still running: their output now.
        for id in std::mem::take(&mut tr.tests) {
            let args = json!({"threadId": thread, "view": "activity", "itemId": id, "maxCharsPerItem": 50_000});
            match tokio::time::timeout(std::time::Duration::from_secs(8), c.call("t3_thread_read", args)).await {
                Ok(Ok(r)) => match r["items"].as_array().and_then(|a| a.first()) {
                    Some(it) if it["status"] == "running" || it["status"] == "inProgress" => tr.tests.push(id),
                    Some(it) => tr.reducer.test_result(it["text"].as_str().unwrap_or("")),
                    None => {}
                },
                _ => tr.tests.push(id),
            }
        }
        let mut after = tr.reducer.position.or_else(|| count.checked_sub(TAIL_ITEMS + 1));
        for _ in 0..PAGES {
            let mut args = json!({"threadId": thread, "view": "activity", "limit": PAGE_ITEMS, "maxCharsPerItem": ITEM_CHARS, "runLimit": 1});
            if let Some(a) = after { args["afterPosition"] = json!(a); }
            let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(8), c.call("t3_thread_read", args)).await else { return false };
            let items = r["items"].as_array().cloned().unwrap_or_default();
            for mut it in items {
                if it["type"] == "command_execution" && it["textTruncated"] == true && it["status"] == "completed"
                    && stages::is_test(&stages::split_command(it["text"].as_str().unwrap_or("")).0) {
                    let tail = json!({"threadId": thread, "view": "activity", "itemId": it["itemId"], "textOffset": it["nextTextOffset"], "maxCharsPerItem": 50_000});
                    if let Ok(Ok(rest)) = tokio::time::timeout(std::time::Duration::from_secs(8), c.call("t3_thread_read", tail)).await {
                        if let Some(more) = rest["items"].as_array().and_then(|a| a.first()).and_then(|x| x["text"].as_str()) {
                            let cut: String = more.chars().rev().take(TEST_TAIL).collect::<Vec<_>>().into_iter().rev().collect();
                            it["text"] = json!(format!("{}\n{cut}", it["text"].as_str().unwrap_or("")));
                        }
                    }
                }
                if it["type"] == "command_execution" && matches!(it["status"].as_str(), Some("running" | "inProgress"))
                    && stages::is_test(&stages::split_command(it["text"].as_str().unwrap_or("")).0) {
                    if let Some(id) = it["itemId"].as_str() { tr.tests.push(id.to_owned()) }
                }
                tr.reducer.item(&it);
                if !matches!(it["type"].as_str(), Some("reasoning" | "checkpoint")) {
                    if let Some(t) = it["text"].as_str() { it["text"] = json!(super::digest::clip(t, 400)) }
                    tr.recent.push_back(it);
                    if tr.recent.len() > RECENT_ITEMS { tr.recent.pop_front(); }
                }
            }
            if r["hasMore"] != true { break }
            after = r["nextPosition"].as_u64().or(tr.reducer.position);
        }
        true
    }
}

fn epoch(t: &Value) -> Option<f64> {
    chrono::DateTime::parse_from_rfc3339(t.as_str()?).ok().map(|t| t.timestamp_millis() as f64 / 1000.0)
}

fn now_epoch() -> f64 { chrono::Utc::now().timestamp_millis() as f64 / 1000.0 }

/// Every question of a `t3_pending_request` read answer, in order (none is dropped, 24 Oct). An item without a
/// question text falls back to its header, then to "Question" (R14): its id must stay in the answer map.
pub fn parse_question(request: &str, q: &Value) -> Option<Question> {
    let items: Vec<Item> = q["questions"].as_array()?.iter().enumerate().filter_map(|(n, x)| {
        let header = x["header"].as_str().map(str::trim).filter(|h| !h.is_empty()).map(str::to_owned);
        // An item with neither text nor header stays (its id keys the answer): it is asked as "Question" (R14).
        let text = x["question"].as_str().map(str::trim).filter(|t| !t.is_empty()).map(str::to_owned).or_else(|| header.clone()).unwrap_or_else(|| "Question".into());
        let options = x["options"].as_array().into_iter().flatten().filter_map(|o| {
            let label = o["label"].as_str().or(o.as_str())?.to_owned();
            let description = o["description"].as_str().map(str::trim).filter(|d| !d.is_empty()).map(str::to_owned);
            Some(Opt { label, description })
        }).collect();
        Some(Item {
            id: x["id"].as_str().map(str::to_owned).unwrap_or_else(|| format!("q{}", n + 1)),
            header,
            text,
            multi_select: x["multiSelect"] == true,
            allow_other: x["allowCustomAnswer"] == true,
            options,
        })
    }).collect();
    (!items.is_empty()).then(|| Question { request: request.into(), items })
}

#[async_trait::async_trait]
impl ThreadSource for T3Source {
    async fn view(&self, thread: &str) -> Result<ThreadView> {
        let c = &self.workers.client;
        // Three runs, so a run T3 dropped before it started is looked past (`current_state`).
        let r = c.call("t3_thread_read", json!({"threadId": thread, "view": "messages", "limit": 1, "runLimit": 3, "maxCharsPerItem": 200})).await?;
        let t = &r["thread"];
        let (run, status, pending, current) = current_state(&r);
        let mut view = ThreadView {
            status,
            title: t["title"].as_str().unwrap_or("").into(),
            pending,
            question: None,
            working_children: 0,
            recent_children: 0,
            ended_recently: current.and_then(|run| run["completedAt"].as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .is_some_and(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_minutes() < 60),
            archived: t["archived"] == true,
            background: 0,
            run,
            new_run: false,
            signals: None,
            raw: None,
            approval: None,
        };
        let items = t["itemCount"].as_u64().unwrap_or(0);
        let still_quiet = self.quiet.lock().unwrap_or_else(|e| e.into_inner()).get(thread)
            .is_some_and(|(n, at)| *n == items && at.elapsed() < std::time::Duration::from_secs(600));
        // The run as code follows it: the same reducer while the run is the same.
        let started = current.and_then(|run| epoch(&run["startedAt"]).or_else(|| epoch(&run["requestedAt"]))).unwrap_or_else(now_epoch);
        let taken = self.tracks.lock().unwrap_or_else(|e| e.into_inner()).remove(thread).filter(|tr| tr.reducer.run == view.run);
        let mut tr = taken.unwrap_or_else(|| Track { reducer: stages::Reducer::new(&view.run, started), count: 0, recent: Default::default(), children: vec![], listed: None, train: None, tests: vec![] });
        let moved = tr.count != items;
        let read_ok = if moved && !view.run.is_empty() { self.read_items(thread, items, &mut tr).await } else { true };
        let active = ACTIVE.contains(&view.status.as_str());
        let mut listed = false;
        if view.status == "completed" && !view.archived {
            view.background = self.workers.background_work(thread).unwrap_or(0) as u64;
        }
        if view.status == "completed" && !view.archived && !still_quiet {
            if let Some(project) = t["projectId"].as_str() {
                let list = c.call("t3_thread_list", json!({"projectId": project, "includeSubagents": true, "limit": CHILD_LIST_LIMIT})).await?;
                let now = chrono::Utc::now();
                let kids: Vec<Value> = children(&list, thread).into_iter().cloned().collect();
                view.working_children = kids.iter().filter(|k| child_working(k, now)).count() as u64;
                // Quiet for minutes but changed within the hour: still the job only while its latest item is not its
                // closing message (it waits on something it started); read, as the plan refresher used to.
                let mut recent = 0;
                for k in kids.iter().filter(|k| recent_child(k, now) && !child_working(k, now)).take(4) {
                    let id = k["threadId"].as_str().unwrap_or("");
                    let n = k["itemCount"].as_u64().unwrap_or(0);
                    let mut args = json!({"threadId": id, "view": "activity", "limit": 6, "maxCharsPerItem": 240, "runLimit": 1});
                    if n > 6 { args["afterPosition"] = json!(n - 7); }
                    let waits = match c.call("t3_thread_read", args).await {
                        Ok(read) => read["items"].as_array().into_iter().flatten().rev()
                            .find(|it| !matches!(it["type"].as_str(), Some("reasoning" | "checkpoint")))
                            .is_some_and(|it| it["type"] != "assistant_message"),
                        // Unreadable: as before, recent counts.
                        Err(_) => true,
                    };
                    if waits { recent += 1 }
                }
                view.recent_children = recent;
                let mut quiet = self.quiet.lock().unwrap_or_else(|e| e.into_inner());
                if settles_quiet(view.working_children, view.recent_children) { quiet.insert(thread.to_owned(), (items, std::time::Instant::now())); } else { quiet.remove(thread); }
                tr.children = kids;
                listed = true;
            }
        } else if view.status != "completed" {
            self.quiet.lock().unwrap_or_else(|e| e.into_inner()).remove(thread);
        }
        // While it runs, its sub-agents are the Sub-agents and Reviewing stages: listed when it moved, else every 2 min.
        if active && (moved || tr.listed.is_none_or(|at| at.elapsed() >= LIST_EVERY)) {
            if let Some(project) = t["projectId"].as_str() {
                if let Ok(list) = c.call("t3_thread_list", json!({"projectId": project, "includeSubagents": true, "limit": CHILD_LIST_LIMIT})).await {
                    tr.children = children(&list, thread).into_iter().cloned().collect();
                    listed = true;
                }
            }
        }
        let now = now_epoch();
        if listed {
            tr.listed = Some(std::time::Instant::now());
            let utc = chrono::Utc::now();
            let open: Vec<String> = tr.children.iter().filter(|k| child_working(k, utc))
                .map(|k| super::digest::without_tag(k["title"].as_str().unwrap_or("sub-agent")).to_owned()).collect();
            tr.reducer.subagents(&open, tr.children.len(), now);
        }
        // A TrainingLab run its commands point at: Training while it is live, Paused while it is stopped.
        if let Some(root) = &self.runs {
            if tr.train.is_none() && (!tr.reducer.runs_named.is_empty() || !tr.reducer.train_hints.is_empty()) {
                tr.train = super::train::find(root, &tr.reducer.runs_named, &tr.reducer.train_hints);
            }
        }
        if let Some(dir) = &tr.train { tr.reducer.training(super::train::read(dir, now, None), now); }
        if view.status == "waiting" || view.pending > 0 { tr.reducer.asking(now); } else { tr.reducer.answered(now); }
        if read_ok || tr.reducer.items > 0 {
            view.signals = Some(tr.reducer.signals(now));
            view.raw = Some(super::digest::ThreadRaw {
                read: json!({"thread": t.clone(), "recentRuns": r["recentRuns"].clone(), "items": tr.recent.iter().cloned().collect::<Vec<_>>()}),
                children: tr.children.clone(),
                child_reads: vec![],
                listed,
            });
        }
        if read_ok { tr.count = items }
        self.tracks.lock().unwrap_or_else(|e| e.into_inner()).insert(thread.to_owned(), tr);
        // An archived thread's open request is moot (`sync_patch` drops it), and reading it could fail the whole sync
        // and freeze the island on its last stage (bug note 2026-11-02T16-27-21: "Reading", "Drafting" stayed).
        if view.archived && !ACTIVE.contains(&view.status.as_str()) { return Ok(view) }

        if view.status == "waiting" || view.pending > 0 {
            if let Some(id) = t3::question_ids(c, thread).await?.first() {
                let q = t3::read_question(c, thread, id).await?;
                view.question = parse_question(id, &q);
            }
            // No question: an approval prompt, as Heimdall's T3 pass recorded it for this run (its card and link).
            view.approval = approval_for(&view, self.workers.approval_wait(thread).unwrap_or_else(|e| {
                eprintln!("islands: approval wait of {thread} not read: {e:#}");
                None
            }));
        }
        Ok(view)
    }
    async fn answer(&self, thread: &str, q: &Question, picks: &[Pick]) -> Result<()> {
        let answers = t3_answers(q, picks)?;
        t3::answer_question(&self.workers.client, thread, &q.request, Value::Object(answers)).await?;
        Ok(())
    }
    async fn cleared(&self, thread: &str) -> Result<Vec<String>> {
        if !self.workers.clear_task(thread)? { return Ok(vec![]) }
        self.workers.archive_cleared().await
    }
}

/// Work for the binding loop from other threads (the tools, the response watcher).
#[derive(Debug)]
pub enum Job {
    /// Re-read one bound island's thread now (just bound).
    Sync(String),
    /// The user completed the notch card of request `request` on `island`: one pick per question.
    Answer { island: String, request: String, picks: Vec<Pick> },
    /// The user closed a job island bound to this thread: its task is archived once finished (never stopped).
    Cleared(String),
}

/// A job island of `thread` left (the user closed it, or it retired after he saw it done): unless another island still
/// shows the thread, its task is archived once finished and reported, now or on a later pass (
/// "figure out why they are not closing themself after the islands responsible for a job have been cleared").
pub async fn island_cleared(bindings: &Bindings, src: &dyn ThreadSource, thread: &str) {
    if bindings.all().iter().any(|(_, b)| b.thread == thread) { return }
    match src.cleared(thread).await {
        Ok(titles) => for t in titles { eprintln!("islands: task '{t}' archived after its island was cleared") },
        Err(e) => eprintln!("islands: task {thread} not archived yet: {}", crate::islands::short_error(&e)),
    }
}

/// T3 no longer has `thread` (deleted): its read fails with `thread_not_found`, "Thread <id> is no longer available."
/// (T3 0.1.6, checked live 2 Nov). A connection failure, or another request that is gone, is not this.
pub fn thread_gone(e: &anyhow::Error, thread: &str) -> bool {
    let t = format!("{e:#}");
    t.starts_with("T3_TOOL_FAILED") && t.contains(&format!("Thread {thread} is no longer available"))
}

/// Bring one bound island in line with its thread. Returns whether it unbound.
pub async fn sync_one(root: &Root, bindings: &Bindings, src: &dyn ThreadSource, island: &str) -> Result<bool> {
    let Some(b) = bindings.get(island) else { return Ok(false) };
    if root.read(island).is_none() {
        // The user closed it or the agent removed it.
        return Ok(bindings.unbind(island));
    }
    let mut view = match src.view(&b.thread).await {
        Ok(view) => view,
        // The thread is gone from T3 (deleted): nothing more can come of it, so its island leaves now rather than
        // failing every pass on its last stage (bug note 2026-11-02T16-27-21).
        Err(e) if thread_gone(&e, &b.thread) => {
            root.locked(|| { if root.read(island).is_some() { std::fs::remove_file(root.file(island))? } Ok(()) })?;
            bindings.unbind(island);
            eprintln!("islands: thread {} is gone from T3; its island '{island}' left", b.thread);
            return Ok(true);
        }
        Err(e) => return Err(e),
    };
    view.new_run = b.run.as_deref().is_some_and(|r| !view.run.is_empty() && r != view.run);
    // A request the user answered is not a question again while T3 still lists it (R6).
    if view.question.as_ref().is_some_and(|q| bindings.recently_answered(island, &q.request)) { view.question = None; }
    if b.question != view.question { bindings.set_question(island, view.question.clone()); }
    // The latest picture the run named, copied into the islands folder once (pictures in /tmp vanish); the island
    // agent decides whether it is the moment's graphic (round 3: a picture older than 15 minutes is stale).
    let picture = view.signals.as_ref().and_then(|g| keep_picture(root, bindings, island, &g.pictures));
    let mut settled = false;
    let after = root.patch(island, |o| {
        let (mut patch, s) = sync_patch(o, &view);
        settled = s;
        if view.signals.is_some() { bindings.only_new(island, &mut patch); }
        if !patch.is_empty() {
            super::files::merge(o, &patch);
            super::tools::stamp_now(o, &patch);
        }
    })?;
    // The island agent gets every bound island's signals after its sync; it decides when a call is due.
    if let (Some(g), Some(o)) = (view.signals.clone(), after) {
        bindings.to_agent(crate::island_agent::Snapshot {
            island: island.to_owned(), thread: b.thread.clone(),
            state: o.get("state").and_then(Value::as_str).unwrap_or("running").to_owned(),
            drawn: o.get("summaryRun").and_then(Value::as_str).is_some_and(|r| !r.is_empty() && r == g.run),
            signals: g, raw: view.raw.clone().unwrap_or_default(),
            image: picture,
            question: view.question.clone(),
        });
    }
    if !view.run.is_empty() && b.run.as_deref() != Some(view.run.as_str()) { bindings.set_run(island, &view.run); }
    // An archived thread that settled is over (task_cancel, or archived by hand): its island leaves the screen now
    // (bug 25 Oct 23:26: five done islands stayed until the user closed them).
    // A real failure stays for the user to see; done and stopped (a cancel) go. Read again and removed under the folder's
    // lock, so a writer between the patch and the removal is not lost.
    if view.archived && settled {
        let gone = root.locked(|| {
            let ends = root.read(island).is_some_and(|o| {
                let (state, stage) = (o.get("state").and_then(Value::as_str), o.get("stage").and_then(Value::as_str));
                state == Some("done") || (state == Some("failed") && stage == Some("Stopped"))
            });
            if ends { std::fs::remove_file(root.file(island))? }
            Ok(ends)
        })?;
        if gone {
            bindings.unbind(island);
            return Ok(true);
        }
    }
    // Settled is not the end: the island stays bound and follows a new run on the same thread.
    Ok(settled)
}

/// How long a done job island stays on screen once the user has seen it: shown (islands not hidden) and closed, counted
/// only while he could see it, or after he opened and closed it (bug 25 Oct 23:26: "not closing themselves").
pub const DONE_SHOWN_FOR: std::time::Duration = std::time::Duration::from_secs(10 * 60);
pub const DONE_AFTER_OPENED: std::time::Duration = std::time::Duration::from_secs(60);

/// One done island's time on screen, for `Retire`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DoneClock {
    /// Time it was shown closed while done.
    pub shown: std::time::Duration,
    /// The user opened it while done; time since he closed it.
    pub since_opened: Option<std::time::Duration>,
}

impl DoneClock {
    /// One tick of `dt`: whether the island is done, every island is shown (not hidden), and this one is open now.
    /// Returns whether it should leave now. An open island never leaves; hidden time does not count.
    pub fn step(&mut self, done: bool, shown: bool, open: bool, dt: std::time::Duration) -> bool {
        if !done {
            *self = DoneClock::default();
            return false;
        }
        if !shown {
            return false;
        }
        if open {
            self.since_opened = Some(std::time::Duration::ZERO);
            return false;
        }
        self.shown += dt;
        if let Some(t) = &mut self.since_opened { *t += dt }
        self.shown >= DONE_SHOWN_FOR || self.since_opened.is_some_and(|t| t >= DONE_AFTER_OPENED)
    }
}

fn is_done(o: &Map<String, Value>) -> bool {
    o.get("state").and_then(Value::as_str) == Some("done") && o.get("attention").is_none_or(Value::is_null)
}

/// The user touched the keyboard, pointer or trackpad within this long: he could be looking at the screen. A locked
/// screen or a Mac left alone counts as unseen.
pub const PRESENT_WITHIN_S: f64 = 60.0;

/// Seconds since the user's last input in this login session (keyboard, pointer, trackpad).
#[cfg(target_os = "macos")]
pub fn idle_seconds() -> f64 {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventSourceSecondsSinceLastEventType(state: i32, event_type: u32) -> f64;
    }
    // SAFETY: a pure query; combined session state (0), any input event type (!0).
    unsafe { CGEventSourceSecondsSinceLastEventType(0, !0u32) }
}
#[cfg(not(target_os = "macos"))]
pub fn idle_seconds() -> f64 { 0.0 }

/// Done job islands leaving by themselves: each bound island's `DoneClock`, stepped from the helper's `state.json`.
#[derive(Default)]
pub struct Retire {
    clocks: BTreeMap<String, DoneClock>,
}

impl Retire {
    /// One tick: every bound island that is done steps its clock; those due are removed and unbound. Returns them,
    /// each with its thread.
    /// `hidden`: every island is hidden, or the user is away (no input within `PRESENT_WITHIN_S`, a locked screen).
    pub fn tick(&mut self, root: &Root, bindings: &Bindings, hidden: bool, dt: std::time::Duration) -> Vec<(String, String)> {
        let screen = root.screen_state();
        let on_screen = screen.get("islands").and_then(Value::as_object);
        let bound: Vec<(String, String)> = bindings.all().into_iter().map(|(id, b)| (id, b.thread)).collect();
        self.clocks.retain(|id, _| bound.iter().any(|(b, _)| b == id));
        let mut gone = vec![];
        for (id, thread) in bound {
            let Some(o) = root.read(&id) else { continue };
            let done = is_done(&o);
            let shown_here = on_screen.and_then(|m| m.get(&id));
            // Not on screen yet (the helper has not drawn it): nothing counts.
            let shown = !hidden && shown_here.is_some();
            let open = shown_here.and_then(|s| s.get("open")).and_then(Value::as_bool).unwrap_or(false);
            if self.clocks.entry(id.clone()).or_default().step(done, shown, open, dt) {
                // Still done at the moment of removal, under the folder's lock: a rewrite meanwhile keeps it.
                let removed = root.locked(|| {
                    let still = root.read(&id).is_some_and(|o| is_done(&o));
                    if still { std::fs::remove_file(root.file(&id))? }
                    Ok(still)
                });
                if removed.unwrap_or(false) {
                    bindings.unbind(&id);
                    gone.push((id.clone(), thread));
                }
                self.clocks.remove(&id);
            }
        }
        gone
    }
}

/// Bind every island whose file names a thread (`"thread": {"id": …}` or a plain id) and is not bound to it yet.
pub fn adopt(root: &Root, bindings: &Bindings) {
    for (id, o) in root.all() {
        let thread = match o.get("thread") {
            Some(Value::Object(t)) => t.get("id").and_then(Value::as_str),
            Some(Value::String(t)) => Some(t.as_str()),
            _ => None,
        };
        if let Some(t) = thread.filter(|t| !t.trim().is_empty()) {
            if bindings.get(&id).is_none_or(|b| b.thread != t) { bindings.bind(&id, t) }
        }
    }
}

/// A job island's title from a task title: 1-3 words naming the thing (islands/docs/island-style.md). The part
/// before a colon when it is short ("Arsenal: push branch" → "Arsenal"), else the first three words without a
/// trailing small word ("Install planned voice pipeline" → "Install planned voice").
pub fn job_title(task: &str) -> String {
    // T3 thread titles carry Heimdall's launch marker; it is never shown.
    let t = task.split(" [heimdall:").next().unwrap_or(task).trim();
    if let Some((head, _)) = t.split_once(':') {
        let n = head.split_whitespace().count();
        if (1..=3).contains(&n) { return head.trim().to_string() }
    }
    let mut words: Vec<&str> = t.split_whitespace().take(3).collect();
    while words.len() > 1 && ["a", "an", "the", "of", "for", "and", "to", "in", "on", "with", "&", "-", "·"].contains(&words[words.len() - 1].to_lowercase().as_str()) {
        words.pop();
    }
    let out = words.join(" ");
    if out.is_empty() { "Task".into() } else { out }
}

/// Every task Heimdall starts gets a running job island bound to its thread at once, docked right and born from the
/// notch (bugs 22 Oct 17:09 and 21:11: "every delegate_task should get a bound island automatically"). A thread
/// that already has an island keeps it. When the brief names the page the task makes, the island carries its open
/// button from the start (`report_link`); an open target already set stays. Returns the note for
/// `delegate_task`'s answer.
pub fn task_island(h: &super::Host, thread: &str, title: &str, brief: &str) -> Option<String> {
    if thread.is_empty() { return None }
    // One at a time: two tasks with the same title started together must not pick the same free id.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let page = super::report_link::output_page(brief);
    if let Some((id, o)) = h.bindings.all().into_iter().find_map(|(id, b)| (b.thread == thread).then(|| h.root.read(&id).map(|o| (id, o))).flatten()) {
        if let Some(p) = page.filter(|_| o.get("open").and_then(Value::as_str).is_none_or(|t| t.trim().is_empty())) {
            if let Err(e) = super::tools::put(h, &json!({"id": id, "open": p})) {
                eprintln!("islands: no open target on '{id}': {}", super::short_error(&e));
            }
        }
        return Some(format!("Its job island is '{id}'; add details with islands_put on that id."));
    }
    let name = job_title(title);
    let base = format!("task-{}", super::files::safe_id(&name.to_lowercase().replace(' ', "-")));
    // Another thread's island may hold the name: the next free one.
    let id = std::iter::once(base.clone()).chain((2..).map(|n| format!("{base}-{n}")))
        .find(|id| h.root.read(id).is_none() && h.bindings.get(id).is_none_or(|b| b.thread == thread))
        .unwrap_or(base);
    let mut a = json!({"id": id, "title": name, "state": "running", "stage": "Starting", "side": "right", "thread": thread});
    // What it was sent to do, once (the task title after its project, "Arsenal 1.0: Promote the allowed models…"), and
    // the goal it serves when its project names exactly one (docs/one-system.md).
    let bare = title.split(" [heimdall:").next().unwrap_or(title).trim();
    let sent = bare.split_once(':').map_or(bare, |(_, rest)| rest).trim();
    let sent: String = if sent.chars().count() > 110 { sent.chars().take(109).collect::<String>() + "\u{2026}" } else { sent.to_owned() };
    if !sent.is_empty() { a["sent"] = json!(sent) }
    if let Some(g) = super::tools::goal_for(&h.root, bare) { a["goal"] = json!(g) }
    if let Some(p) = page { a["open"] = json!(p) }
    let r = super::tools::put(h, &a).or_else(|_| { a["title"] = json!("Task"); super::tools::put(h, &a) });
    match r {
        Ok(_) => Some(format!("Its job island '{id}' follows the thread; add details with islands_put on that id.")),
        Err(e) => { eprintln!("islands: no job island for thread {thread}: {}", super::short_error(&e)); None }
    }
}

/// Answer the open request of a bound island from its notch card. Returns the notice line for the agent.
pub async fn answer(root: &Root, bindings: &Bindings, src: &dyn ThreadSource, island: &str, request: &str, picks: &[Pick]) -> Result<String> {
    let title = root.read(island).and_then(|o| o.get("title").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| island.to_string());
    let b = bindings.get(island).context("the island is no longer bound to a thread")?;
    let q = b.question.filter(|q| q.request == request).context("that question is no longer open")?;
    let said: Vec<String> = q.items.iter().map(|item| {
        let pick = picks.iter().find(|p| p.question_id == item.id);
        let mut parts: Vec<String> = pick.map(|p| p.labels.clone()).unwrap_or_default();
        parts.extend(pick.and_then(|p| p.other.clone()));
        if parts.is_empty() { return format!("nothing to '{}'", item.text) }
        format!("'{}' to '{}'", parts.join("' and '"), item.text)
    }).collect();
    // Checked before anything is sent: a card that does not match the request is refused, not half-sent. The card
    // already left the notch when the user answered it, so a refusal of any kind puts the request back for the next sync
    // to show again (R4).
    let sent = match t3_answers(&q, picks) {
        Ok(_) => src.answer(&b.thread, &q, picks).await,
        Err(e) => Err(e),
    };
    match sent {
        Ok(()) => {
            bindings.set_question(island, None);
            bindings.remember_answered(island, &q.request);
            root.patch(island, |o| {
                let patch: Map<String, Value> = [
                    ("state".to_string(), json!("running")), ("stage".to_string(), json!("Working")),
                    ("attention".to_string(), Value::Null), ("parts".to_string(), parts_with(o, vec![])),
                ].into_iter().collect();
                super::files::merge(o, &patch);
                super::tools::stamp_now(o, &patch);
            })?;
            Ok(format!("User answered {} on the island '{title}'; sent to T3 thread {}.", said.join("; "), b.thread))
        }
        Err(e) => {
            bindings.set_question(island, None);
            Ok(format!("User answered {} on the island '{title}', but T3 refused the answer: {}", said.join("; "), crate::islands::short_error(&e)))
        }
    }
}

/// How often done islands are checked for leaving (`Retire`): no T3 call, two small file reads.
const RETIRE_TICK: std::time::Duration = std::time::Duration::from_secs(15);

/// The binding loop: runs beside the T3 watcher until `stop`.
pub async fn run(
    host: Arc<super::Host>,
    src: Arc<dyn ThreadSource>,
    mut passes: tokio::sync::watch::Receiver<u64>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut jobs: tokio::sync::mpsc::UnboundedReceiver<Job>,
) {
    let sync_all = |host: Arc<super::Host>, src: Arc<dyn ThreadSource>| async move {
        adopt(&host.root, &host.bindings);
        for (island, _) in host.bindings.all() {
            if let Err(e) = sync_one(&host.root, &host.bindings, src.as_ref(), &island).await {
                eprintln!("islands: thread sync for {island}: {}", crate::islands::short_error(&e));
            }
        }
    };
    sync_all(host.clone(), src.clone()).await;
    let mut retire = Retire::default();
    let mut tick = tokio::time::interval(RETIRE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            r = stop.changed() => { if r.is_err() || *stop.borrow() { break } }
            _ = tick.tick() => {
                let away = super::visibility::hidden() || idle_seconds() > PRESENT_WITHIN_S;
                for (id, thread) in retire.tick(&host.root, &host.bindings, away, RETIRE_TICK) {
                    eprintln!("islands: done island '{id}' left after User saw it");
                    island_cleared(&host.bindings, src.as_ref(), &thread).await;
                }
            }
            job = jobs.recv() => match job {
                None => break,
                Some(Job::Sync(island)) => {
                    if let Err(e) = sync_one(&host.root, &host.bindings, src.as_ref(), &island).await {
                        eprintln!("islands: thread sync for {island}: {}", crate::islands::short_error(&e));
                    }
                }
                Some(Job::Cleared(thread)) => island_cleared(&host.bindings, src.as_ref(), &thread).await,
                Some(Job::Answer { island, request, picks }) => {
                    let line = answer(&host.root, &host.bindings, src.as_ref(), &island, &request, &picks).await
                        .unwrap_or_else(|e| format!("User clicked an answer on the island '{island}', but it could not be sent: {}", crate::islands::short_error(&e)));
                    host.deliver(super::Class::Context, &line);
                    let _ = sync_one(&host.root, &host.bindings, src.as_ref(), &island).await;
                }
            },
            r = passes.changed() => { if r.is_err() { break } sync_all(host.clone(), src.clone()).await }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }
    fn view(status: &str, pending: u64, q: Option<Question>) -> ThreadView {
        ThreadView { status: status.into(), title: "T".into(), pending, question: q, ..Default::default() }
    }
    fn opt(label: &str) -> Opt {
        Opt { label: label.into(), description: None }
    }
    fn item(id: &str, text: &str, multi: bool, other: bool, options: &[&str]) -> Item {
        Item { id: id.into(), header: None, text: text.into(), multi_select: multi, allow_other: other, options: options.iter().map(|l| opt(l)).collect() }
    }
    fn q() -> Question {
        Question { request: "r1".into(), items: vec![item("q1", "Which voice?", false, false, &["Warm", "Bright"])] }
    }
    fn pick(id: &str, labels: &[&str], other: Option<&str>) -> Pick {
        Pick { question_id: id.into(), labels: labels.iter().map(|l| l.to_string()).collect(), other: other.map(str::to_owned) }
    }

    #[test]
    fn a_question_is_attention_and_its_text_and_leaves_agent_parts_alone() {
        let island = obj(json!({"title": "Voices", "state": "running", "stage": "Rendering", "parts": [{"type": "stat", "value": "3"}]}));
        let (p, settled) = sync_patch(&island, &view("waiting", 1, Some(q())));
        assert!(!settled);
        assert_eq!((p["state"].as_str(), p["stage"].as_str(), p["attention"].as_str()), (Some("waiting"), Some("Your answer"), Some("question")));
        let parts = p["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2, "the answer is on the notch card, not here: no buttons");
        assert_eq!(parts[0]["type"], "stat");
        assert_eq!(parts[1], json!({"type": "text", "value": "Which voice?", "t3q": true}));
    }

    #[test]
    fn running_keeps_the_agents_stage_clears_the_question_and_unchanged_writes_nothing() {
        let mut island = obj(json!({"state": "waiting", "stage": "Your answer", "attention": "question",
            "parts": [{"type": "text", "value": "Which voice?", "t3q": true}, {"type": "button", "label": "Warm", "action": "t3q:r1:0"}]}));
        let (p, _) = sync_patch(&island, &view("running", 0, None));
        assert_eq!(p["stage"], "Working");
        assert_eq!(p["parts"], Value::Null);
        assert_eq!(p["attention"], Value::Null);
        crate::islands::files::merge(&mut island, &p);
        island.insert("stage".into(), json!("Final tests"));
        let (p, _) = sync_patch(&island, &view("running", 0, None));
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn the_refreshers_stage_survives_polls_and_changes_only_with_the_state() {
        let island = obj(json!({"state": "waiting", "stage": "Voice pick", "attention": "question",
            "parts": [{"type": "text", "value": "Which voice?", "t3q": true}]}));
        assert!(sync_patch(&island, &view("waiting", 1, Some(q()))).0.is_empty());
        let (p, _) = sync_patch(&island, &view("completed", 0, None));
        assert_eq!(p["stage"], "Done");
    }

    #[test]
    fn a_completed_turn_with_sub_agents_at_work_stays_running_and_bound() {
        let island = obj(json!({"state": "running", "stage": "Model tests"}));
        let mut v = view("completed", 0, None);
        v.working_children = 3;
        let (p, settled) = sync_patch(&island, &v);
        assert!(!settled);
        assert!(p.is_empty(), "nothing changes while the workers run: {p:?}");
        let done = obj(json!({"state": "done", "stage": "Done"}));
        let (p, settled) = sync_patch(&done, &v);
        assert_eq!((p["state"].as_str(), p["stage"].as_str(), settled), (Some("running"), Some("Working"), false));
        // The turn just ended (background work may go on): bound, silent, the refresher reads the job.
        let fresh = ThreadView { working_children: 0, recent_children: 0, ended_recently: true, ..v.clone() };
        assert_eq!(sync_patch(&island, &fresh), (Map::new(), false));
        // Quiet for over 4 minutes but changed within the hour (a worker waiting on a GPU lock): bound, silent.
        let quiet = ThreadView { working_children: 0, recent_children: 1, ..v.clone() };
        assert_eq!(sync_patch(&island, &quiet), (Map::new(), false));
        // …but a question the user answered meanwhile does not linger.
        let asked = obj(json!({"state": "waiting", "stage": "Your answer", "attention": "question",
            "parts": [{"type": "text", "value": "Which?", "t3q": true}, {"type": "button", "label": "A", "action": "t3q:r:0"}, {"type": "stat", "value": "3"}]}));
        let (p, settled) = sync_patch(&asked, &quiet);
        assert!(!settled && p["attention"].is_null() && p.contains_key("attention"));
        assert_eq!((p["state"].as_str(), p["stage"].as_str()), (Some("running"), Some("Working")), "answered: not waiting any more");
        assert_eq!(p["parts"], json!([{"type": "stat", "value": "3"}]));
        // An hour of silence: settled.
        let gone = ThreadView { working_children: 0, recent_children: 0, ..v };
        assert!(sync_patch(&island, &gone).1);
        // Waiting on the user's pick (the refresher's, no T3 question): kept until a new run.
        let pick = obj(json!({"state": "waiting", "stage": "Your pick"}));
        assert_eq!(sync_patch(&pick, &gone), (Map::new(), true));
        let failed = obj(json!({"state": "failed", "stage": "Build broke"}));
        assert_eq!(sync_patch(&failed, &gone), (Map::new(), true), "a failure the refresher read is kept, not flipped to done");
        assert_eq!(sync_patch(&pick, &view("running", 0, None)).0["state"], "running");
    }

    #[test]
    fn an_archived_thread_ends_its_island_at_once() {
        // Bugs 23 Oct 22:12 and 23:05: tasks closed with task_cancel kept "Starting" / running for the hour.
        let starting = obj(json!({"state": "running", "stage": "Starting"}));
        let archived = |status: &str| ThreadView { ended_recently: true, recent_children: 2, working_children: 1, archived: true, ..view(status, 0, None) };
        let (p, settled) = sync_patch(&starting, &archived("completed"));
        assert_eq!((p["state"].as_str(), p["stage"].as_str(), settled), (Some("done"), Some("Done"), true));
        let (p, settled) = sync_patch(&starting, &archived("interrupted"));
        assert_eq!((p["state"].as_str(), p["stage"].as_str(), settled), (Some("failed"), Some("Stopped"), true));
        assert_eq!(sync_patch(&starting, &archived("idle")).0["stage"], "Stopped", "archived before it ever ran");
        assert_eq!(sync_patch(&starting, &archived("failed")).0["stage"], "Failed");
        // A question left open on an archived thread goes with it; nobody can answer it any more.
        let asked = obj(json!({"state": "waiting", "stage": "Your answer", "attention": "question",
            "parts": [{"type": "text", "value": "Which?", "t3q": true}, {"type": "button", "label": "A", "action": "t3q:r:0"}]}));
        let (p, settled) = sync_patch(&asked, &ThreadView { pending: 1, question: Some(q()), ..archived("completed") });
        assert!(settled && p["attention"].is_null() && p["parts"].is_null() && p["state"] == "done", "{p:?}");
        // A failure the refresher read stays; once done, nothing is rewritten.
        let failed = obj(json!({"state": "failed", "stage": "Build broke"}));
        assert_eq!(sync_patch(&failed, &archived("completed")), (Map::new(), true));
        let done = obj(json!({"state": "done", "stage": "Done"}));
        assert_eq!(sync_patch(&done, &archived("completed")), (Map::new(), true));
        // A run started on it again (a steer unarchives it in T3, but even if not): the island follows it.
        assert_eq!(sync_patch(&done, &archived("running")).0["state"], "running");
    }

    #[test]
    fn sub_agents_count_as_working_only_while_they_change() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-22T12:30:00Z").unwrap().with_timezone(&chrono::Utc);
        let list = json!({"threads": [
            {"threadId": "c1", "parentThreadId": "p", "status": "idle", "settled": false, "updatedAt": "2026-10-22T12:28:30Z"},
            {"threadId": "c2", "parentThreadId": "p", "status": "idle", "settled": false, "updatedAt": "2026-10-22T11:44:50Z"},
            {"threadId": "c3", "parentThreadId": "p", "status": "running", "updatedAt": "2026-10-22T10:00:00Z"},
            {"threadId": "c4", "parentThreadId": "p", "status": "idle", "settled": true, "updatedAt": "2026-10-22T12:29:00Z"},
            // Finished with an approval T3 never closed (bug note 2026-11-02T14-56-51): no wait, not working.
            {"threadId": "c5", "parentThreadId": "p", "status": "completed", "settled": true, "pendingRequestCount": 1, "updatedAt": "2026-10-22T12:29:00Z"},
            {"threadId": "x", "parentThreadId": "other", "status": "running"},
            {"threadId": "p", "parentThreadId": null, "status": "completed"}]});
        let kids = children(&list, "p");
        assert_eq!(kids.len(), 5);
        let working: Vec<&str> = kids.into_iter().filter(|k| child_working(k, now)).map(|k| k["threadId"].as_str().unwrap()).collect();
        assert_eq!(working, ["c1", "c3"]);
        let recent: Vec<&str> = children(&list, "p").into_iter().filter(|k| recent_child(k, now)).map(|k| k["threadId"].as_str().unwrap()).collect();
        assert_eq!(recent, ["c1", "c2"], "c2 changed 45 min ago: recent; c3 not for 2.5 h; c4 settled");
    }

    #[test]
    fn settles_done_failed_stopped_but_not_with_an_open_question() {
        let island = obj(json!({"state": "running"}));
        let (p, s) = sync_patch(&island, &view("completed", 0, None));
        assert!(s);
        assert_eq!((p["state"].as_str(), p["stage"].as_str()), (Some("done"), Some("Done")));
        assert_eq!(sync_patch(&island, &view("failed", 0, None)).0["state"], "failed");
        assert_eq!(sync_patch(&island, &view("interrupted", 0, None)).0["stage"], "Stopped");
        let (p, s) = sync_patch(&island, &view("completed", 1, Some(q())));
        assert!(!s);
        assert_eq!(p["state"], "waiting");
        let (p, s) = sync_patch(&island, &view("completed", 1, None));
        assert!(!s);
        assert_eq!(p["attention"], "waiting");
        assert!(sync_patch(&island, &view("idle", 0, None)).0.is_empty());
    }

    #[test]
    fn parses_every_question_of_a_request_in_order_with_its_options_and_flags() {
        let q = parse_question("r9", &json!({"questions": [
            {"id": "q1", "header": "Voice", "question": "Which one?", "options": [{"label": "A", "description": "x"}, "B"]},
            {"id": "q2", "header": "Tones", "question": "Which tones?", "multiSelect": true, "allowCustomAnswer": true,
             "options": [{"label": "Warm", "description": ""}, {"label": "Bright", "description": "crisper"}, {"label": "Dark"}]},
            {"header": "Headed only", "options": ["Yes", "No"]},
            {"question": "", "header": "  ", "options": ["Kept"]}
        ]})).unwrap();
        assert_eq!(q.request, "r9");
        assert_eq!(q.items.len(), 4, "an item with no text is kept, asked as \"Question\"; none is dropped");
        assert_eq!((q.items[3].id.as_str(), q.items[3].text.as_str(), q.items[3].options[0].label.as_str()), ("q4", "Question", "Kept"));
        assert_eq!(q.items[0], Item { id: "q1".into(), header: Some("Voice".into()), text: "Which one?".into(), multi_select: false, allow_other: false,
            options: vec![Opt { label: "A".into(), description: Some("x".into()) }, Opt { label: "B".into(), description: None }] });
        assert!(q.items[1].multi_select && q.items[1].allow_other, "multiSelect and allowCustomAnswer carry over");
        assert_eq!(q.items[1].options[0].description, None, "an empty description is none");
        assert_eq!(q.items[1].options[1].description.as_deref(), Some("crisper"));
        assert_eq!(q.items[2].id, "q3", "an item with no id is keyed by its place");
        assert_eq!(q.items[2].text, "Headed only", "no question text falls back to the header");
        assert!(parse_question("r", &json!({})).is_none());
        assert!(parse_question("r", &json!({"questions": []})).is_none());
    }

    #[test]
    fn t3_answers_keys_each_question_by_id_and_refuses_a_pick_the_card_cannot_make() {
        let q = Question { request: "r1".into(), items: vec![
            item("q1", "Which voice?", false, false, &["Warm", "Bright"]),
            item("q2", "Which tones?", true, true, &["Warm", "Bright", "Dark"]),
        ] };
        let ok = t3_answers(&q, &[pick("q2", &["Warm", "Dark"], Some(" soft ")), pick("q1", &["Bright"], None)]).unwrap();
        assert_eq!(Value::Object(ok), json!({"q1": "Bright", "q2": ["Warm", "Dark", "soft"]}), "single: a string; multi: an array, his words last");
        let typed = Question { request: "r2".into(), items: vec![item("q1", "Name it?", false, true, &["Yes", "No"])] };
        assert_eq!(Value::Object(t3_answers(&typed, &[pick("q1", &[], Some("Later"))]).unwrap()), json!({"q1": "Later"}));
        // Sent with q2 left (bug note 2026-11-02T16-27-21): it takes words, so it goes as unanswered; q1 takes only
        // its options, so it cannot be left; nothing answered is refused.
        assert_eq!(Value::Object(t3_answers(&q, &[pick("q1", &["Bright"], None)]).unwrap()), json!({"q1": "Bright", "q2": ["(unanswered)"]}));
        assert_eq!(Value::Object(t3_answers(&q, &[pick("q1", &["Bright"], None), pick("q2", &[], Some("  "))]).unwrap())["q2"], json!(["(unanswered)"]));
        assert_eq!(t3_answers(&typed, &[pick("q1", &[], None)]).unwrap_err().to_string(), "nothing answered");
        assert!(t3_answers(&q, &[pick("q2", &["Warm"], None)]).is_err(), "q1 takes only its options");
        assert!(t3_answers(&q, &[pick("q1", &["Other"], None), pick("q2", &["Warm"], None)]).is_err(), "a label that is not offered");
        assert!(t3_answers(&q, &[pick("q1", &["Bright"], Some("and")), pick("q2", &["Warm"], None)]).is_err(), "a single question takes one answer");
        let no_other = Question { request: "r3".into(), items: vec![item("q1", "Which voice?", false, false, &["Warm", "Bright"])] };
        assert!(t3_answers(&no_other, &[pick("q1", &[], Some("Dark"))]).is_err(), "no Other row, no words of his own");
    }

    #[test]
    fn a_request_is_one_card_on_the_notch_until_it_is_answered_or_its_island_unbinds() {
        let sent = Arc::new(Mutex::new(Vec::<ui::UiUpdate>::new()));
        let bindings = Bindings::load(None);
        let into = sent.clone();
        bindings.set_card_sink(Arc::new(move |u| into.lock().unwrap().push(u)));
        bindings.bind("voices", "th");
        bindings.set_question("voices", Some(q()));
        {
            let shown = sent.lock().unwrap();
            match shown.as_slice() {
                [ui::UiUpdate::AgentNotice(n)] => {
                    assert_eq!(n.id, "t3:voices:r1");
                    match &n.action {
                        Some(ui::agent_notice::NoticeAction::Ask { request_id, questions, .. }) => {
                            assert_eq!(request_id, "t3:voices:r1");
                            assert_eq!((questions.len(), questions[0].id.as_str(), questions[0].options.len()), (1, "q1", 2));
                        }
                        other => panic!("{other:?}"),
                    }
                }
                other => panic!("{} updates", other.len()),
            }
        }
        bindings.set_question("voices", Some(q()));
        assert_eq!(sent.lock().unwrap().len(), 1, "the same request again shows nothing new");
        bindings.set_question("voices", None);
        assert!(matches!(sent.lock().unwrap().last(), Some(ui::UiUpdate::DismissAgentNotice(id)) if id == "t3:voices:r1"));
        bindings.set_question("voices", Some(q()));
        assert!(bindings.unbind("voices"));
        assert!(matches!(sent.lock().unwrap().last(), Some(ui::UiUpdate::DismissAgentNotice(id)) if id == "t3:voices:r1"), "unbinding takes the card with it");
    }

    #[test]
    fn a_card_id_names_its_island_and_request_and_nothing_else() {
        assert_eq!(card_id("voices", "req:1"), "t3:voices:req:1");
        assert_eq!(parse_card_id("t3:voices:req:1"), Some(("voices".into(), "req:1".into())));
        assert_eq!(parse_card_id("ask-1"), None);
        assert_eq!(parse_card_id("t3:voices"), None);
    }

    #[test]
    fn bindings_persist_across_loads() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("b.json");
        let b = Bindings::load(Some(path.clone()));
        b.bind("voices", "thread-1");
        b.set_question("voices", Some(q()));
        let again = Bindings::load(Some(path));
        assert_eq!(again.get("voices").unwrap().question, Some(q()));
        assert!(again.unbind("voices"));
        assert!(Bindings::load(Some(d.path().join("b.json"))).get("voices").is_none());
    }

    struct Fake { view: Mutex<ThreadView>, answered: Mutex<Vec<(String, String, Vec<Pick>)>> }
    #[async_trait::async_trait]
    impl ThreadSource for Fake {
        async fn view(&self, _: &str) -> Result<ThreadView> { Ok(self.view.lock().unwrap().clone()) }
        async fn answer(&self, thread: &str, q: &Question, picks: &[Pick]) -> Result<()> {
            self.answered.lock().unwrap().push((thread.into(), q.request.clone(), picks.to_vec()));
            Ok(())
        }
    }

    /// Every notch update for requests, in order: a card's id when it is shown, `-id` when it is dismissed.
    fn notch_log(bindings: &Bindings) -> Arc<Mutex<Vec<String>>> {
        let log = Arc::new(Mutex::new(Vec::new()));
        let into = log.clone();
        bindings.set_card_sink(Arc::new(move |u| match u {
            ui::UiUpdate::AgentNotice(n) => into.lock().unwrap().push(n.id),
            ui::UiUpdate::DismissAgentNotice(id) => into.lock().unwrap().push(format!("-{id}")),
            _ => {}
        }));
        log
    }

    /// T3 takes the respond only to refuse it (R4).
    struct Refusing { view: ThreadView }
    #[async_trait::async_trait]
    impl ThreadSource for Refusing {
        async fn view(&self, _: &str) -> Result<ThreadView> { Ok(self.view.clone()) }
        async fn answer(&self, _: &str, _: &Question, _: &[Pick]) -> Result<()> { anyhow::bail!("T3 refused the respond") }
    }

    /// T3 lost the thread (deleted): each read fails with thread_not_found (bug note 2026-11-02T16-27-21).
    struct Gone(&'static str);
    #[async_trait::async_trait]
    impl ThreadSource for Gone {
        async fn view(&self, _: &str) -> Result<ThreadView> { anyhow::bail!("{}", self.0) }
        async fn answer(&self, _: &str, _: &Question, _: &[Pick]) -> Result<()> { Ok(()) }
    }

    /// A deleted thread's island leaves at its next sync, with its card; T3 being down or a lost request does not
    /// take an island away (bug note 2026-11-02T16-27-21: islands of finished tasks stayed on "Reading").
    #[tokio::test]
    async fn a_deleted_threads_island_leaves_and_an_unreachable_t3_keeps_it() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        let log = notch_log(&bindings);
        root.put("audit", &obj(json!({"title": "Audit T3", "state": "running", "stage": "Reading"}))).unwrap();
        bindings.bind("audit", "mcp:1");
        bindings.set_question("audit", Some(q()));
        let down = Gone("T3_UNAVAILABLE: open T3 Code; discovery file is unavailable");
        assert!(sync_one(&root, &bindings, &down, "audit").await.is_err());
        let other = Gone(r#"T3_TOOL_FAILED: {"_tag":"OrchestratorMcpFailure","code":"request_not_found","message":"Request r1 is no longer available."}"#);
        assert!(sync_one(&root, &bindings, &other, "audit").await.is_err());
        assert!(root.read("audit").is_some() && bindings.get("audit").is_some(), "still there while T3 cannot say");
        let gone = Gone(r#"T3_TOOL_FAILED: {"_tag":"OrchestratorMcpFailure","code":"thread_not_found","message":"Thread mcp:1 is no longer available."}"#);
        assert!(sync_one(&root, &bindings, &gone, "audit").await.unwrap(), "unbound");
        assert!(root.read("audit").is_none() && bindings.get("audit").is_none());
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("-t3:audit:r1"), "its card goes with it");
        assert!(thread_gone(&anyhow::anyhow!("T3_TOOL_FAILED: Thread mcp:9 is no longer available."), "mcp:9"));
        assert!(!thread_gone(&anyhow::anyhow!("T3_TOOL_FAILED: Thread mcp:9 is no longer available."), "mcp:1"));
    }

    /// A clear takes the notch's cards with it; the bindings forget the request, so the next sync shows it again (R1).
    #[tokio::test]
    async fn a_cleared_notch_shows_the_open_request_again_at_the_next_sync() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        let log = notch_log(&bindings);
        root.put("voices", &obj(json!({"title": "Voices", "state": "running"}))).unwrap();
        bindings.bind("voices", "th");
        let fake = Fake { view: Mutex::new(view("waiting", 1, Some(q()))), answered: Mutex::new(vec![]) };
        sync_one(&root, &bindings, &fake, "voices").await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["t3:voices:r1".to_string()]);
        // The agent turned off or restarted: the notch lost its cards, and clear forgets the open requests.
        bindings.forget_questions();
        assert!(bindings.get("voices").unwrap().question.is_none());
        sync_one(&root, &bindings, &fake, "voices").await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["t3:voices:r1".to_string(), "t3:voices:r1".to_string()], "shown again");
    }

    /// A refused respond puts the request back: the card left the notch when the user answered it, so the next sync shows
    /// it again instead of leaving T3's question with no card (R4).
    #[tokio::test]
    async fn a_refused_answer_shows_the_request_again_at_the_next_sync() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        let log = notch_log(&bindings);
        root.put("voices", &obj(json!({"title": "Voices", "state": "running"}))).unwrap();
        bindings.bind("voices", "th");
        let src = Refusing { view: view("waiting", 1, Some(q())) };
        sync_one(&root, &bindings, &src, "voices").await.unwrap();
        let line = answer(&root, &bindings, &src, "voices", "r1", &[pick("q1", &["Bright"], None)]).await.unwrap();
        assert!(line.contains("refused"), "{line}");
        assert!(bindings.get("voices").unwrap().question.is_none(), "the request is put back for the next sync");
        sync_one(&root, &bindings, &src, "voices").await.unwrap();
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("t3:voices:r1"), "{:?}", log.lock().unwrap());
        assert_eq!(log.lock().unwrap().iter().filter(|e| *e == "t3:voices:r1").count(), 2, "shown before and again after the refusal");
    }

    /// T3 may keep listing a request after the user answered it: it does not come back as a new question during the
    /// memory window, and it does once the window has passed (R6).
    #[tokio::test]
    async fn an_answered_request_stays_out_of_the_notch_while_t3_still_lists_it() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        let log = notch_log(&bindings);
        root.put("voices", &obj(json!({"title": "Voices", "state": "running"}))).unwrap();
        bindings.bind("voices", "th");
        let fake = Fake { view: Mutex::new(view("waiting", 1, Some(q()))), answered: Mutex::new(vec![]) };
        sync_one(&root, &bindings, &fake, "voices").await.unwrap();
        answer(&root, &bindings, &fake, "voices", "r1", &[pick("q1", &["Bright"], None)]).await.unwrap();
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("-t3:voices:r1"), "the answered card leaves the notch");
        sync_one(&root, &bindings, &fake, "voices").await.unwrap();
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("-t3:voices:r1"), "T3 still lists r1: not shown again");
        assert_ne!(root.read("voices").unwrap()["attention"], "question", "nor on the floating island");
        // Once the memory has passed, a request T3 still lists is a question again.
        let past = std::time::Instant::now().checked_sub(ANSWERED_MEMORY + std::time::Duration::from_secs(1)).unwrap();
        bindings.answered.lock().unwrap().insert(card_id("voices", "r1"), past);
        sync_one(&root, &bindings, &fake, "voices").await.unwrap();
        assert_eq!(log.lock().unwrap().last().map(String::as_str), Some("t3:voices:r1"));
    }

    #[tokio::test]
    async fn sync_answer_and_settle_through_a_fake_thread() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        root.put("voices", &obj(json!({"title": "Voices", "state": "running"}))).unwrap();
        bindings.bind("voices", "th");
        let fake = Fake { view: Mutex::new(view("waiting", 1, Some(q()))), answered: Mutex::new(vec![]) };
        assert!(!sync_one(&root, &bindings, &fake, "voices").await.unwrap());
        assert_eq!(root.read("voices").unwrap()["attention"], "question");
        let bright = [pick("q1", &["Bright"], None)];
        let line = answer(&root, &bindings, &fake, "voices", "r1", &bright).await.unwrap();
        assert_eq!(line, "User answered 'Bright' to 'Which voice?' on the island 'Voices'; sent to T3 thread th.");
        assert_eq!(fake.answered.lock().unwrap()[0], ("th".into(), "r1".into(), bright.to_vec()));
        let o = root.read("voices").unwrap();
        assert_eq!((o["state"].as_str(), o.get("parts"), o.get("attention")), (Some("running"), None, None));
        // A stale click (the question already answered) is refused plainly.
        assert!(answer(&root, &bindings, &fake, "voices", "r1", &bright).await.unwrap_err().to_string().contains("no longer open"));
        *fake.view.lock().unwrap() = view("completed", 0, None);
        assert!(sync_one(&root, &bindings, &fake, "voices").await.unwrap());
        assert_eq!(root.read("voices").unwrap()["state"], "done");
        assert!(bindings.get("voices").is_some(), "a finished thread stays followed");
        // A steer starts a new run on the same thread (the user 15:19, "Mistral vs Heimdall"): the island follows it live.
        *fake.view.lock().unwrap() = view("running", 0, None);
        assert!(!sync_one(&root, &bindings, &fake, "voices").await.unwrap());
        let o = root.read("voices").unwrap();
        assert_eq!((o["state"].as_str(), o.get("ended")), (Some("running"), None), "no green check while it works again");
        assert_eq!(o["stage"], "Working");
    }

    #[tokio::test]
    async fn a_cancelled_tasks_island_settles_on_the_next_sync() {
        let d = tempfile::tempdir().unwrap();
        let h = crate::islands::Host::new(Root(d.path().to_path_buf()), None);
        task_island(&h, "th", "Ack lines", "").unwrap();
        let ended = ThreadView { ended_recently: true, ..view("completed", 0, None) };
        let fake = Fake { view: Mutex::new(ended.clone()), answered: Mutex::new(vec![]) };
        assert!(!sync_one(&h.root, &h.bindings, &fake, "task-ack-lines").await.unwrap());
        assert_eq!(h.root.read("task-ack-lines").unwrap()["stage"], "Starting", "before archiving: the refresher's to read");
        *fake.view.lock().unwrap() = ThreadView { archived: true, ..ended };
        assert!(sync_one(&h.root, &h.bindings, &fake, "task-ack-lines").await.unwrap());
        // Bug 25 Oct 23:26: archived and settled, the island leaves at once and unbinds.
        assert!(h.root.read("task-ack-lines").is_none(), "an archived task's island is removed");
        assert!(h.bindings.get("task-ack-lines").is_none());
        // A real failure stays for the user to see; a cancel (Stopped) goes.
        for (status, stays) in [("failed", true), ("interrupted", false)] {
            task_island(&h, "th2", "Grok CLI", "").unwrap();
            *fake.view.lock().unwrap() = ThreadView { archived: true, ..view(status, 0, None) };
            sync_one(&h.root, &h.bindings, &fake, "task-grok-cli").await.unwrap();
            assert_eq!(h.root.read("task-grok-cli").is_some(), stays, "{status}");
            let _ = h.root.remove("task-grok-cli");
            h.bindings.unbind("task-grok-cli");
        }
    }

    #[tokio::test]
    async fn an_archived_thread_running_again_keeps_its_island() {
        let d = tempfile::tempdir().unwrap();
        let h = crate::islands::Host::new(Root(d.path().to_path_buf()), None);
        task_island(&h, "th", "Ack lines", "").unwrap();
        let fake = Fake { view: Mutex::new(ThreadView { archived: true, ..view("running", 0, None) }), answered: Mutex::new(vec![]) };
        assert!(!sync_one(&h.root, &h.bindings, &fake, "task-ack-lines").await.unwrap());
        assert_eq!(h.root.read("task-ack-lines").unwrap()["state"], "running");
    }

    /// Bug note 2026-10-28T18-55-56: a stage written for one run stayed on the island while a send's new run did
    /// other work. A new run starts over at "Working"; the same run keeps the stage it was given.
    #[tokio::test]
    async fn a_new_run_drops_the_last_runs_stage() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        root.put("nemo", &obj(json!({"title": "Nemotron", "state": "running", "stage": "Checkpoint download"}))).unwrap();
        bindings.bind("nemo", "th");
        let at = |status: &str, run: &str| ThreadView { run: run.into(), ..view(status, 0, None) };
        let fake = Fake { view: Mutex::new(at("running", "r1")), answered: Mutex::new(vec![]) };
        sync_one(&root, &bindings, &fake, "nemo").await.unwrap();
        assert_eq!(root.read("nemo").unwrap()["stage"], "Checkpoint download", "the same run keeps its stage");
        *fake.view.lock().unwrap() = ThreadView { ended_recently: true, ..at("completed", "r1") };
        sync_one(&root, &bindings, &fake, "nemo").await.unwrap();
        *fake.view.lock().unwrap() = at("running", "r2");
        sync_one(&root, &bindings, &fake, "nemo").await.unwrap();
        let o = root.read("nemo").unwrap();
        assert_eq!((o["state"].as_str(), o["stage"].as_str()), (Some("running"), Some("Working")), "a new run starts over");
        // The refresher's stage for the new run stays.
        root.patch("nemo", |o| { o.insert("stage".into(), json!("Quantising")); }).unwrap();
        sync_one(&root, &bindings, &fake, "nemo").await.unwrap();
        assert_eq!(root.read("nemo").unwrap()["stage"], "Quantising");
    }

    /// Code-driven islands (30 Oct): signals from the reducer, at a moment of a run that started at t=1000.
    fn signals(stage: &str) -> stages::Signals {
        let mut r = stages::Reducer::new("run-1", 1000.0);
        let at = |s: f64| chrono::DateTime::from_timestamp_millis(((1000.0 + s) * 1000.0) as i64).unwrap().to_rfc3339();
        r.item(&json!({"position": 1, "runId": "run-1", "type": "assistant_message", "text": "The build is running. Next a probe.", "updatedAt": at(20.0)}));
        r.item(&json!({"position": 2, "runId": "run-1", "type": "command_execution", "text": "$ cargo build -p app\n Compiling", "updatedAt": at(30.0)}));
        if stage == "Testing" { r.item(&json!({"position": 3, "runId": "run-1", "type": "command_execution", "text": "$ cargo test -p app\ntest result: ok. 12 passed", "updatedAt": at(400.0)})); }
        r.signals(1000.0 + 600.0)
    }

    #[test]
    fn with_signals_code_names_the_stage_clock_and_band_and_ends_at_once() {
        let island = obj(json!({"title": "Release", "state": "running", "stage": "Starting", "parts": [{"type": "line", "points": [1, 2], "islandAgent": true}]}));
        let v = ThreadView { signals: Some(signals("Building")), ..view("running", 0, None) };
        let (p, settled) = sync_patch(&island, &v);
        assert!(!settled);
        assert_eq!(p["stage"], "Building", "within the pass, no model");
        assert_eq!(p["started"], json!(1000.0), "the island clock is the run's");
        assert_eq!(p["summary"], "The build is running");
        let parts = p["parts"].as_array().unwrap();
        assert_eq!(parts[0]["islandAgent"], true, "the island agent's graphic stays, first");
        let band = parts.last().unwrap();
        assert!(is_band(band) && band["state"] == "running");
        assert_eq!(band["segments"].as_array().unwrap().last().unwrap(), &json!({"title": "Building", "secs": 570.0}), "the ribbon ends in the current stage");
        assert!(!p.contains_key("steps") && !p.contains_key("kv"), "round 3: no rail, no bookkeeping counters");
        // The island agent wrote this run's now line for this moment: code's first sentence steps back.
        let mut agent = island.clone();
        agent.insert("summary".into(), json!("Building the release"));
        agent.insert("summaryRun".into(), json!("run-1"));
        agent.insert("summaryStage".into(), json!("Preparing"));
        assert!(!sync_patch(&agent, &v).0.contains_key("summary"));
        // A question: Your answer, and the history ends in it.
        let q = ThreadView { signals: Some(signals("Building")), ..view("waiting", 1, Some(q())) };
        let (p, _) = sync_patch(&island, &q);
        assert_eq!((p["stage"].as_str(), p["attention"].as_str()), (Some("Your answer"), Some("question")));
        let parts = p["parts"].as_array().unwrap();
        assert_eq!(parts[parts.len() - 2]["t3q"], true, "the question text, then the band");
        assert_eq!(parts.last().unwrap()["segments"].as_array().unwrap().last().unwrap()["title"], "Your answer");
        // The turn ended with nothing open: done now, not after the hour; the history all done; no progress.
        let ended = ThreadView { ended_recently: true, signals: Some(signals("Testing")), ..view("completed", 0, None) };
        let (p, settled) = sync_patch(&island, &ended);
        assert_eq!((p["state"].as_str(), p["stage"].as_str(), settled), (Some("done"), Some("Done"), true));
        let band = p["parts"].as_array().unwrap().last().unwrap().clone();
        assert_eq!((band["state"].as_str(), band["segments"].as_array().unwrap().last().unwrap()["title"].as_str()), (Some("done"), Some("Testing")), "a done ribbon has no Done segment (the renderer ends it in a check)");
        assert!(!p.contains_key("progress") || p["progress"].is_null());
        // Failed: the ribbon ends in a red Failed stage.
        let (p, _) = sync_patch(&island, &ThreadView { signals: Some(signals("Testing")), ..view("failed", 0, None) });
        assert_eq!(p["parts"].as_array().unwrap().last().unwrap()["segments"].as_array().unwrap().last().unwrap()["title"], "Failed");
        // A background command it started has not reported back, or a sub-agent may still wait: still running.
        let mut bg = signals("Testing");
        bg.background_open = 1;
        let (p, settled) = sync_patch(&island, &ThreadView { ended_recently: true, signals: Some(bg), ..view("completed", 0, None) });
        assert_eq!((p.get("state"), p["stage"].as_str(), settled), (None, Some("Background"), false), "still running: {p:?}");
        let (p, _) = sync_patch(&island, &ThreadView { ended_recently: true, recent_children: 1, signals: Some(signals("Testing")), ..view("completed", 0, None) });
        assert_eq!(p["stage"], "Sub-agents");
        // Without signals (the items unreadable this pass): the old rule, the hour is waited out.
        assert_eq!(sync_patch(&island, &ThreadView { ended_recently: true, ..view("completed", 0, None) }), (Map::new(), false));
        // Bug note 2026-11-03T22-49-42: the latest run's items show nothing open (a new wake run, past the hour), but
        // Heimdall's record of the task still has a background restore and its monitor running: not finished.
        let (p, settled) = sync_patch(&island, &ThreadView { background: 2, signals: Some(signals("Testing")), ..view("completed", 0, None) });
        let state = p.get("state").or(island.get("state")).and_then(Value::as_str);
        assert_eq!((state, p.get("stage").and_then(Value::as_str), settled), (Some("running"), Some("Background"), false), "{p:?}");
        // Without signals: still running and not settled (the stage changes only with the state, as before); an island
        // that had said done goes back to running Background.
        let (p, settled) = sync_patch(&island, &ThreadView { background: 1, ..view("completed", 0, None) });
        assert_eq!((p.get("state"), settled), (None, false), "{p:?}");
        let done = obj(json!({"title": "Release", "state": "done", "stage": "Done"}));
        let (p, settled) = sync_patch(&done, &ThreadView { background: 1, ..view("completed", 0, None) });
        assert_eq!((p.get("state").and_then(Value::as_str), p.get("stage").and_then(Value::as_str), settled), (Some("running"), Some("Background"), false), "{p:?}");
        // A run the plan refresher marked waiting on the user keeps that mark while its monitor runs.
        let picked = obj(json!({"title": "Release", "state": "waiting", "stage": "Your pick"}));
        assert_eq!(sync_patch(&picked, &ThreadView { background: 1, ..view("completed", 0, None) }), (Map::new(), true));
        // Once they end, the same view is done.
        let (p, settled) = sync_patch(&island, &ThreadView { background: 0, signals: Some(signals("Testing")), ..view("completed", 0, None) });
        assert_eq!((p.get("state").and_then(Value::as_str), settled), (Some("done"), true));
    }

    /// Review, 30 Oct: with Islands off, no quota or a failed End call, a done island kept the agent's running line.
    /// Code takes the line back once the moment it was written for is over, and writes its own at the end.
    #[test]
    fn code_takes_the_now_line_back_when_the_agents_moment_is_over() {
        let agent_line = |stage: &str| obj(json!({"title": "Release", "state": "running", "stage": "Testing", "summary": "Downloading the 9 GB voice weights",
            "summaryRun": "run-1", "summaryStage": stage}));
        let testing = ThreadView { signals: Some(signals("Testing")), ..view("running", 0, None) };
        assert!(!sync_patch(&agent_line("Testing"), &testing).0.contains_key("summary"), "its moment: the agent's line stands");
        let (p, _) = sync_patch(&agent_line("Downloading"), &testing);
        assert_eq!(p["summary"], "The build is running", "the stage moved on: code's line");
        assert!(p["summaryRun"].is_null() && p.contains_key("summaryRun"), "and the marker goes");
        let ended = ThreadView { signals: Some(signals("Testing")), ..view("completed", 0, None) };
        let (p, _) = sync_patch(&agent_line("Testing"), &ended);
        assert_eq!((p["state"].as_str(), p["summary"].as_str()), (Some("done"), Some("The build is running")), "no End call yet: code's own line at the end");
        let mut done = agent_line("end");
        done.insert("state".into(), json!("done"));
        done.insert("stage".into(), json!("Done"));
        assert!(!sync_patch(&done, &ended).0.contains_key("summary"), "the agent's end line stands");
        // Taking back a line code wrote before is written all the same (code's memory of its last write).
        let b = Bindings::load(None);
        let mut p1 = obj(json!({"summary": "The build is running"}));
        b.only_new("rel", &mut p1);
        let mut p2 = obj(json!({"summary": "The build is running", "summaryRun": null}));
        b.only_new("rel", &mut p2);
        assert!(p2.contains_key("summary"));
    }

    /// Review, 30 Oct: working sub-agents are not "recent", so a completed parent whose children all worked was marked
    /// quiet, and the next pass skipped listing them and declared the job done.
    #[test]
    fn a_completed_thread_whose_sub_agents_all_work_is_never_quiet() {
        assert!(!settles_quiet(3, 0), "all children working");
        assert!(!settles_quiet(0, 1));
        assert!(settles_quiet(0, 0));
        // And the island stays running while they work.
        let island = obj(json!({"state": "running", "stage": "Sub-agents"}));
        let v = ThreadView { working_children: 3, signals: Some(signals("Building")), ..view("completed", 0, None) };
        let (p, settled) = sync_patch(&island, &v);
        assert!(!settled && p.get("state").is_none(), "{p:?}");
    }

    struct Coded { view: Mutex<ThreadView> }
    #[async_trait::async_trait]
    impl ThreadSource for Coded {
        async fn view(&self, _: &str) -> Result<ThreadView> { Ok(self.view.lock().unwrap().clone()) }
        async fn answer(&self, _: &str, _: &Question, _: &[Pick]) -> Result<()> { Ok(()) }
    }

    #[tokio::test]
    async fn code_writes_its_keys_only_when_they_change_and_hands_the_stored_picture_to_the_island_agent() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("fi"));
        let bindings = Bindings::load(None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        bindings.set_agent(Some(tx));
        root.put("rel", &obj(json!({"title": "Release", "state": "running", "summary": "Shipping 1.4"}))).unwrap();
        bindings.bind("rel", "th");
        let shot = d.path().join("shot.png");
        std::fs::write(&shot, b"\x89PNG\r\n\x1a\n0000000000").unwrap();
        let mut g = signals("Building");
        g.summary = None;
        g.pictures = vec!["/tmp/gone-already.png".into(), shot.to_string_lossy().into_owned()];
        let src = Coded { view: Mutex::new(ThreadView { run: "run-1".into(), signals: Some(g.clone()), ..view("running", 0, None) }) };
        sync_one(&root, &bindings, &src, "rel").await.unwrap();
        let o = root.read("rel").unwrap();
        assert_eq!(o["stage"], "Building");
        assert_eq!(o["summary"], "Shipping 1.4", "code has no line yet: the agent's stays");
        assert!(o.get("image").is_none(), "the island agent decides whether the picture is the moment's graphic");
        let snap = rx.try_recv().unwrap();
        let copy = snap.image.clone().unwrap();
        assert!(copy.starts_with(&root.0.join("pictures").to_string_lossy().into_owned()) && std::path::Path::new(&copy).is_file(), "copied into the islands folder: {copy}");
        assert_eq!((snap.island.as_str(), snap.state.as_str()), ("rel", "running"));
        // The main agent names the stage itself: it stands until code's stage changes.
        root.patch("rel", |o| { o.insert("stage".into(), json!("Final build")); }).unwrap();
        sync_one(&root, &bindings, &src, "rel").await.unwrap();
        assert_eq!(root.read("rel").unwrap()["stage"], "Final build");
        src.view.lock().unwrap().signals = Some(signals("Testing"));
        sync_one(&root, &bindings, &src, "rel").await.unwrap();
        let o = root.read("rel").unwrap();
        assert_eq!((o["stage"].as_str(), o["summary"].as_str()), (Some("Testing"), Some("The build is running")));
        assert_eq!(rx.try_recv().unwrap().image.as_deref(), Some(copy.as_str()), "the copy stays while the picture does");
        assert_eq!(rx.try_recv().unwrap().image, None, "signals without a picture offer none");
        // A new picture replaces the old copy; one picture per island in the store.
        let next = d.path().join("next.jpg");
        std::fs::write(&next, b"\xff\xd8\xff\xe0 0000000000").unwrap();
        let mut g = signals("Testing");
        g.pictures = vec![next.to_string_lossy().into_owned()];
        src.view.lock().unwrap().signals = Some(g);
        sync_one(&root, &bindings, &src, "rel").await.unwrap();
        assert!(rx.try_recv().unwrap().image.unwrap().ends_with(".jpg"));
        assert_eq!(std::fs::read_dir(root.0.join("pictures")).unwrap().count(), 1, "the older copy went");
    }

    #[test]
    fn a_done_island_leaves_after_ten_shown_minutes_or_a_minute_after_owner_opened_it() {
        use std::time::Duration as D;
        let m = D::from_secs(60);
        let mut c = DoneClock::default();
        // Hidden time and running time do not count.
        assert!(!c.step(true, false, false, 30 * m));
        assert!(!c.step(false, true, false, 30 * m));
        for _ in 0..9 { assert!(!c.step(true, true, false, m)) }
        assert!(c.step(true, true, false, m), "ten minutes shown closed");
        // Opened: never while open; a minute after he closes it.
        let mut c = DoneClock::default();
        assert!(!c.step(true, true, true, 20 * m));
        assert!(!c.step(true, true, false, D::from_secs(45)));
        assert!(c.step(true, true, false, D::from_secs(15)));
        // Back to running resets it.
        let mut c = DoneClock::default();
        assert!(!c.step(true, true, false, 9 * m));
        assert!(!c.step(false, true, false, m));
        assert!(!c.step(true, true, false, 9 * m));
    }

    #[test]
    fn retire_removes_only_done_islands_owner_could_see() {
        let d = tempfile::tempdir().unwrap();
        let h = crate::islands::Host::new(Root(d.path().to_path_buf()), None);
        for (id, state) in [("task-a", "done"), ("task-b", "failed"), ("task-c", "done"), ("task-d", "running")] {
            h.root.put(id, &obj(json!({"title": "T", "state": state}))).unwrap();
            h.bindings.bind(id, &format!("th-{id}"));
        }
        // task-c is not on screen yet (the helper has not drawn it).
        let state = json!({"islands": {"task-a": {"open": false}, "task-b": {"open": false}, "task-d": {"open": false}}});
        crate::islands::files::write_atomic(&h.root.state(), &state).unwrap();
        let mut r = Retire::default();
        let ten = std::time::Duration::from_secs(600);
        assert!(r.tick(&h.root, &h.bindings, true, ten).is_empty(), "hidden: nothing counts");
        assert_eq!(r.tick(&h.root, &h.bindings, false, ten), vec![("task-a".to_string(), "th-task-a".to_string())], "with its thread, for the archive");
        assert!(h.root.read("task-a").is_none() && h.bindings.get("task-a").is_none());
        for id in ["task-b", "task-c", "task-d"] { assert!(h.root.read(id).is_some(), "{id} stays") }
    }

    /// Records which threads were handed over for archiving.
    #[derive(Default)]
    struct Clears { calls: Mutex<Vec<String>> }
    #[async_trait::async_trait]
    impl ThreadSource for Clears {
        async fn view(&self, _: &str) -> Result<ThreadView> { Ok(view("completed", 0, None)) }
        async fn answer(&self, _: &str, _: &Question, _: &[Pick]) -> Result<()> { Ok(()) }
        async fn cleared(&self, thread: &str) -> Result<Vec<String>> { self.calls.lock().unwrap().push(thread.into()); Ok(vec![]) }
    }

    /// a job island the user closed hands its thread over for archiving; while another island still
    /// shows the same thread, nothing is handed over.
    #[tokio::test]
    async fn a_cleared_job_island_hands_its_thread_over_unless_another_island_shows_it() {
        let bindings = Bindings::load(None);
        let src = Clears::default();
        island_cleared(&bindings, &src, "th-1").await;
        assert_eq!(*src.calls.lock().unwrap(), vec!["th-1".to_string()]);
        bindings.bind("voices-detail", "th-2");
        island_cleared(&bindings, &src, "th-2").await;
        assert_eq!(src.calls.lock().unwrap().len(), 1, "still shown on 'voices-detail'");
    }

    #[test]
    fn islands_naming_a_thread_are_adopted_by_the_binding() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        let bindings = Bindings::load(None);
        root.put("mistral", &obj(json!({"title": "Mistral vs Heimdall", "state": "done", "thread": {"id": "mcp:3bf4", "title": "x"}}))).unwrap();
        root.put("plain", &obj(json!({"title": "Plain", "thread": "mcp:aa"}))).unwrap();
        root.put("ipad", &obj(json!({"title": "iPad sale"}))).unwrap();
        adopt(&root, &bindings);
        let all: Vec<(String, String)> = bindings.all().into_iter().map(|(i, b)| (i, b.thread)).collect();
        assert_eq!(all, vec![("mistral".into(), "mcp:3bf4".into()), ("plain".into(), "mcp:aa".into())]);
    }

    #[tokio::test]
    async fn a_closed_island_unbinds_without_reading_the_thread() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().to_path_buf());
        let bindings = Bindings::load(None);
        bindings.bind("gone", "th");
        let fake = Fake { view: Mutex::new(view("running", 0, None)), answered: Mutex::new(vec![]) };
        assert!(sync_one(&root, &bindings, &fake, "gone").await.unwrap());
        assert!(bindings.all().is_empty());
    }

    #[test]
    fn job_titles_are_one_to_three_words_naming_the_thing() {
        assert_eq!(job_title("Arsenal: push branch"), "Arsenal");
        assert_eq!(job_title("Install planned voice pipeline"), "Install planned voice");
        assert_eq!(job_title("Tako page v2 [heimdall:abc123]"), "Tako page v2");
        assert_eq!(job_title("Port of the islands"), "Port");
        assert_eq!(job_title("Heimdall voice lane: ack, bench"), "Heimdall voice lane");
        assert_eq!(job_title("  "), "Task");
    }

    #[test]
    fn every_started_task_gets_one_bound_running_island_from_the_notch() {
        let d = tempfile::tempdir().unwrap();
        let h = crate::islands::Host::new(Root(d.path().to_path_buf()), None);
        let note = task_island(&h, "th-1", "Tako page v2", "").unwrap();
        assert!(note.contains("'task-tako-page-v2'"), "{note}");
        let o = h.root.read("task-tako-page-v2").unwrap();
        assert_eq!((o["title"].as_str(), o["state"].as_str(), o["side"].as_str()), (Some("Tako page v2"), Some("running"), Some("right")));
        assert_eq!(o["thread"]["id"], "th-1");
        assert_eq!(o["origin"]["from"], "notch");
        assert_eq!(h.bindings.get("task-tako-page-v2").unwrap().thread, "th-1");
        // A replay, or the agent's own islands_put on another id for the same thread: no second island.
        assert!(task_island(&h, "th-1", "Tako page v2", "").unwrap().contains("'task-tako-page-v2'"));
        assert_eq!(h.root.all().len(), 1);
        // Another task with the same title gets its own island.
        assert!(task_island(&h, "th-2", "Tako page v2", "").unwrap().contains("'task-tako-page-v2-2'"));
        assert_eq!(h.bindings.get("task-tako-page-v2-2").unwrap().thread, "th-2");
        // A thread the agent already showed under its own id keeps that island.
        crate::islands::tools::put(&h, &json!({"id": "voices", "title": "Male voices", "thread": "th-3"})).unwrap();
        assert!(task_island(&h, "th-3", "Voices", "").unwrap().contains("'voices'"));
        assert!(h.root.read("task-voices").is_none());
        assert!(task_island(&h, "", "x", "").is_none());
    }

    #[test]
    fn a_brief_naming_its_page_puts_the_open_button_on_the_island_from_the_start() {
        let d = tempfile::tempdir().unwrap();
        let h = crate::islands::Host::new(Root(d.path().to_path_buf()), None);
        // A page that does not exist yet is bound all the same.
        let brief = "Read /tmp/in/style.html for the look. Write the report to /tmp/out/voices.html.";
        task_island(&h, "th-1", "Voices report", brief).unwrap();
        assert_eq!(h.root.read("task-voices-report").unwrap()["open"], "/tmp/out/voices.html");
        // Only inputs, or no page: no open target.
        task_island(&h, "th-2", "Look", "Make it look like /tmp/in/bench.html").unwrap();
        assert!(h.root.read("task-look").unwrap().get("open").is_none());
        // A thread the agent already showed with its own open target keeps it.
        crate::islands::tools::put(&h, &json!({"id": "costs", "title": "Costs", "thread": "th-3", "open": "/tmp/mine.html"})).unwrap();
        task_island(&h, "th-3", "Costs", "Write /tmp/out/costs.html").unwrap();
        assert_eq!(h.root.read("costs").unwrap()["open"], "/tmp/mine.html");
        // One shown without an open target gets the page.
        crate::islands::tools::put(&h, &json!({"id": "sale", "title": "Sale", "thread": "th-4"})).unwrap();
        task_island(&h, "th-4", "Sale", "Output: /tmp/out/sale.html").unwrap();
        assert_eq!(h.root.read("sale").unwrap()["open"], "/tmp/out/sale.html");
    }

    /// Bug note 2026-11-04T18-30-26: T3 says "running" while an approval prompt holds the run. With the wait Heimdall's
    /// T3 pass recorded for this run, the island reads Needs approval, with the card's text and the button to the
    /// thread; once the prompt is gone (no live request) or the run moved on, it does not.
    #[test]
    fn a_running_thread_with_an_approval_prompt_needs_approval_with_its_card_and_link() {
        let wait = |run: &str| Some(t3::ApprovalWait { run: run.into(), text: Some("rm -rf build/ (clean the stale build)".into()), link: Some("t3-thread://v2/env/beauty".into()) });
        let live = ThreadView { run: "r3".into(), ..view("running", 1, None) };
        let a = approval_for(&live, wait("r3")).expect("the run's own wait");
        assert_eq!(approval_for(&ThreadView { pending: 0, ..live.clone() }, wait("r3")), None, "the prompt is gone");
        assert_eq!(approval_for(&live, wait("r2")), None, "an older run's wait");
        assert_eq!(approval_for(&live, None), None);
        let island = obj(json!({"title": "Beauty", "state": "running", "stage": "Rendering", "parts": [{"type": "stat", "value": "3"}]}));
        let (p, settled) = sync_patch(&island, &ThreadView { approval: Some(a), ..live.clone() });
        assert!(!settled);
        assert_eq!((p["state"].as_str(), p["stage"].as_str(), p["attention"].as_str()), (Some("waiting"), Some("Needs approval"), Some("waiting")));
        assert_eq!(p["parts"], json!([{"type": "stat", "value": "3"},
            {"type": "text", "value": "rm -rf build/ (clean the stale build)", "t3q": true},
            {"type": "openButton", "target": "t3-thread://v2/env/beauty", "t3q": true}]));
        // Approved: the run goes on, and Heimdall's parts go with the wait.
        let mut island = island;
        crate::islands::files::merge(&mut island, &p);
        let (p, _) = sync_patch(&island, &ThreadView { pending: 0, ..live });
        assert_eq!((p["state"].as_str(), p["parts"].clone()), (Some("running"), json!([{"type": "stat", "value": "3"}])));
    }

    /// Bug note 2026-11-04T22-25-15: a turn T3 queued and cancelled before it started made the task read cancelled
    /// (the island: Stopped) while the earlier run worked on. The island follows the run that works.
    #[test]
    fn a_turn_t3_dropped_before_it_started_does_not_stop_the_island() {
        let run6 = json!({"runId": "r6", "ordinal": 6, "status": "running", "startedAt": "2026-11-04T21:22:19Z", "pendingRequestCount": 0});
        let run7 = json!({"runId": "r7", "ordinal": 7, "status": "cancelled", "startedAt": null, "completedAt": "2026-11-04T21:22:37Z"});
        let r = json!({"thread": {"status": "cancelled", "latestRunId": "r7", "activeRunId": "r6", "pendingRequestCount": 1}, "recentRuns": [run7, run6]});
        let (run, status, pending, current) = current_state(&r);
        assert_eq!((run.as_str(), status.as_str(), pending), ("r6", "running", 1));
        assert_eq!(current.unwrap()["runId"], "r6", "the clock and the end follow the working run");
        let island = obj(json!({"title": "Setup wizard", "state": "running", "stage": "Working"}));
        let (p, settled) = sync_patch(&island, &ThreadView { run: run.clone(), ..view(&status, 0, None) });
        assert!(!settled && p.get("stage").is_none_or(|s| s != "Stopped"), "{p:?}");
        let (p, _) = sync_patch(&island, &view("cancelled", 0, None));
        assert_eq!(p["stage"], "Stopped", "T3's own state would have stopped it");
    }
}
