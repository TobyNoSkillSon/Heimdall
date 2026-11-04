//! The plan refresher (night 1022, amended 01:36; its own job since 30 Oct): a background Claude Haiku 5.5 pass keeps
//! the plan islands Scheduled and Settled in step with what the user and Heimdall said and did, so Heimdall carries no
//! bookkeeping. To do moved to Goals (`docs/goals.md`): another job keeps what needs the user and his goals,
//! and this one never reads or writes `todo.json` or `goals.json`. It runs every 5 minutes, skipped when nothing new
//! happened, and when the agent calls the non-blocking `refresh_plan` tool (requests during a pass coalesce into one
//! follow-up pass). Model passes are at
//! least `PERIOD` apart, however often changes or requests arrive: at most 12 an hour. Only the user's own words or a request
//! use that lane; notices, Heimdall's lines, islands and the note wait 15 minutes, 30 once the user has been quiet for an
//! hour (`SLOW_LANES`, night 1027), and an item it added within the hour leaves only when two passes drop it
//! (`hold_young`), unless the user's words or a request set off the pass. Each pass logs what moved. The same pass rebuilds
//! the job islands that follow no thread (work outside a thread, e.g. an iPad sale: title, state, stage, a one-line
//! summary, steps, progress, key facts, a picture and an open target) from the conversation.
//!
//! Thread-bound job islands are not this job's ("each background job type stays separate and
//! stateless"): code drives them within one T3 pass (`islands::binding`, `islands::stages`) and the island agent adds
//! their graphic (`island_agent`). The refresher no longer reads their threads (most of its input before); each shows
//! in its prompt as one read-only line (id, title, state, stage), so the pass knows the work is shown.
//! "Something new" ignores the refresher's own notices (`plan-refresh-…`, context: they never wake the agent),
//! session bookkeeping and a bound island's own churn (only its title and state count); the session file is read only
//! when its size or mtime changed.
//! The user's old-shape plan islands (no `preset`/`list`, plain-string items) are migrated in place on first contact,
//! after a one-time backup of the islands folder (`islands::plan::migrate_all`).
//!
//! Input (read-only): the agent's whole current Pi session, as Pi builds the model's context (the latest compaction
//! summary, whole, and every entry since:, not a trimmed tail), the two lists (`islands::files`), the unbound job
//! islands whole and the bound ones as lines, and the newest `Assistant/night-pass-*.md` in the home folder (personal
//! edition).
//! Model: Background Tasks ▸ Model (`background_model`; `claude-haiku-5-5` by default; Background Tasks ▸ Plan Lists
//! turns the passes off), adaptive
//! thinking at effort high, structured JSON output, through the shared background call path (`background_call`: the
//! system prompt, schema and conversation cached for an hour, the clock and live state last, the model's whole output,
//! at most $0.50 a run, usage and cost logged and counted in the spend ledger). No credential for the chosen model:
//! one log line, nothing read, no pass.
//! Output: validated (a malformed answer is logged and ignored; it never empties the lists), applied through
//! `islands::files` (only `items` on the lists; on unbound job islands only the keys above, every one checked:
//! style-doc lengths, a picture only when it is a real image file the conversation named, never over an open
//! question), then one notice to the agent with a short diff, only when the plan changed.
//! Contract: `docs/archive/briefs/night-1022-islands.md` ([refresher]); the job model: `docs/background-jobs.md`.

use crate::islands::{digest::{one_line, pictures_in}, files::Root, plan as lists};
use crate::pi_session::{active_context, render_entry};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MODEL: &str = agent::provider::HAIKU;
pub const EFFORT: &str = "high";
pub const PERIOD: Duration = Duration::from_secs(5 * 60);
/// Lanes (night 1027: 337 passes in two days, ~12 an hour around the clock, ~90k input tokens each). The user's own words
/// or a `refresh_plan` request run a pass `PERIOD` after the last one; anything else that moved (notices, Heimdall's
/// own lines, bound threads, islands, the night note) waits `SLOW_LANES` gaps, and `QUIET_LANES` gaps once the user has
/// said nothing for `QUIET_AFTER` (a night build's thread moving all night costs two passes an hour, not twelve).
pub const SLOW_LANES: u32 = 3;
pub const QUIET_LANES: u32 = 6;
pub const QUIET_AFTER: Duration = Duration::from_secs(60 * 60);
/// The notice id prefix (`agent::notice::NoticeKind::classify`: context, never a wake).
pub const NOTICE_PREFIX: &str = crate::pi_session::REFRESHER_NOTICE;
pub const STARTED: &str = "Refreshing the plan.";
/// Whole request deadline (the shared background client's: a long answer at the model's full output fits).
pub const TIMEOUT: Duration = crate::background_call::TIMEOUT;
/// The log and ledger label.
pub const JOB: &str = "plan refresher";

/// The plan islands this job keeps, in dock order: (island id, title). To do left for Goals (31 Oct): never here.
pub const LISTS: [(&str, &str); 2] = [("scheduled", "Scheduled"), ("settled", "Settled")];
/// Settled is the Goals shadow's since 2 Nov (the user: "the shadow job that runs Goals should also write Settled"): the
/// refresher reads it, never shows it to the model and never writes it.
pub const SETTLED: usize = 1;
/// Caps on what one answer may write.
pub const LIST_CAP: [usize; 2] = [30, 40];
pub const TITLE_CHARS: usize = 120;
pub const WHEN_CHARS: usize = 40;
pub const DETAIL_CHARS: usize = 600;
pub const STAGE_CHARS: usize = 24;
/// One line (islands/docs/island-style.md).
pub const SUMMARY_CHARS: usize = 110;
pub const MAX_JOBS: usize = 12;
/// A job island's timeline: the current phase only.
pub const STEPS_CAP: usize = 7;
pub const STEP_CHARS: usize = 40;
pub const BARS_CAP: usize = 6;
pub const PARTS_CAP: usize = 3;
const LINE_POINTS: usize = 40;
pub const FACTS_CAP: usize = 4;
/// Marks the parts the refresher wrote (replaced on the next pass; every other part stays).
const OWN_PART: &str = "refresher";
/// The `progress` value the refresher wrote: only a progress still equal to it is the refresher's to clear.
const OWN_PROGRESS: &str = "refresherProgress";

/// Input budget in characters (about 4 per token). The conversation is the whole session since the last compaction
/// up to this ceiling (~85k tokens, keeping a pass under Haiku's 100k-token price step with the rest of the prompt);
/// an agent with a large context budget can go past it before compacting, and then the oldest lines go first, a
/// whole `TRIM_LINES` at a time.
const CONVERSATION_CEILING: usize = 340_000;
/// Over the ceiling, the oldest lines go this many at a time, counted from the compaction: the conversation's start
/// (and with it the cached prefix) stays put for the next several hundred lines instead of moving with each one.
const TRIM_LINES: usize = 10 * CHUNK_LINES;
/// The conversation reaches the model in chunks of this many lines, counted from the compaction: between compactions
/// the session only grows, so every complete chunk is byte-identical from pass to pass and the prompt cache keeps
/// the conversation up to the newest complete chunk (and up to the newest line when only islands or threads moved).
pub const CHUNK_LINES: usize = 40;
const NOTE_BUDGET: usize = 16_000;

// ---------------------------------------------------------------------------------------------------------
// Configuration and the background loop.

/// Where the pass reaches the model; read at each pass (a key added later is picked up).
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// `https://api.anthropic.com` (tests: a local fake).
    pub base: String,
    pub key: Option<String>,
    /// `(header, workspace id)` for keys that name a workspace.
    pub workspace: Option<(String, String)>,
    pub timeout: Duration,
    /// The model the call names, taken with the key in one read of the background model
    /// (`background_model::endpoint`), so a choice changed mid-pass never pairs a new model with the old key.
    /// `None` (tests' endpoints): `background_model::anthropic_model()` at the call.
    pub model: Option<String>,
    /// The request format (`background_call::Wire`): Anthropic Messages for every job's endpoint; deep search's
    /// (`background_model::tool_endpoint`) may name another (any provider the user picks there).
    pub wire: crate::background_call::Wire,
    /// The provider id (`agent::provider`, or `ui::LOCAL_ENGINE`): Anthropic list prices and the $0.50 limit apply only
    /// to `ANTHROPIC` on the Messages format; any other is counted in rounds, without dollars. Also names the provider
    /// in errors.
    pub provider: &'static str,
    /// The model's output maximum when its catalog entry gives one (a Messages request off Anthropic needs one).
    pub max_output: Option<u64>,
    /// The model reasons: an OpenAI chat-completions request carries the job's effort as `reasoning_effort` (Responses
    /// always carries it, Messages as `output_config.effort`).
    pub reasoning: bool,    /// The reasoning efforts the model's catalog entry lists (`agent::provider::ApiModel::efforts`); empty when unknown.
    pub efforts: Vec<String>,
}

impl Default for Endpoint {
    fn default() -> Endpoint {
        Endpoint { base: String::new(), key: None, workspace: None, timeout: Duration::ZERO, model: None,
                   wire: Default::default(), provider: agent::provider::ANTHROPIC, max_output: None, reasoning: false, efforts: vec![] }
    }
}

impl Endpoint {
    /// The Anthropic provider's stored key (the environment's for lab binaries, the Keychain's in the installed app).
    pub fn anthropic() -> Endpoint {
        let p = agent::provider::provider(agent::provider::ANTHROPIC).expect("Anthropic provider");
        Endpoint {
            base: p.anthropic_base.unwrap_or("https://api.anthropic.com").to_owned(),
            key: agent::provider::api_key(p).ok(),
            workspace: p.workspaces.zip(agent::provider::workspace_id(p)).map(|(w, id)| (w.header.to_owned(), id)),
            timeout: TIMEOUT,
            model: None,
            ..Endpoint::default()
        }
    }
}

#[derive(Clone)]
pub struct Config {
    /// Heimdall's data folder (the agent's session lives here).
    pub data: PathBuf,
    pub islands: Root,
    /// The folder holding `night-pass-*.md`.
    pub notes: PathBuf,
    pub endpoint: Arc<dyn Fn() -> Endpoint + Send + Sync>,
    /// False while the agent is off: no pass runs.
    pub enabled: Arc<dyn Fn() -> bool + Send + Sync>,
}

/// What a pass saw last: the conversation (its rendered lines, without the refresher's own notices), the night note
/// and the islands.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Fingerprint {
    session: u64,
    /// The user's own words in it (transcripts, typed messages, drops): not notices, not Heimdall's lines.
    owner: u64,
    note: u64,
    islands: u64,
}

/// The session file as `stat` sees it: path, length, mtime. Unchanged means nothing new was said.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SessionStat(PathBuf, u64, Option<std::time::SystemTime>);

#[derive(Clone, Debug, PartialEq)]
struct Seen {
    fp: Fingerprint,
    stat: SessionStat,
}

#[derive(Default)]
struct Shared {
    wake: tokio::sync::Notify,
    /// Loops alive (a reload starts a new one before the old runtime is gone).
    attached: AtomicU64,
    last: Mutex<Option<Seen>>,
    /// Passes that reached the model (tests).
    calls: AtomicU64,
    /// Session reads (tests: an unchanged file is not read).
    reads: AtomicU64,
    /// When the last pass reached the model, and the least time between two such passes.
    last_call: Mutex<Option<std::time::Instant>>,
    gap_ms: AtomicU64,
    /// The lists' folder, for `request` to put a closed plan island back at once.
    islands: Mutex<Option<Root>>,
    /// The slow and quiet lanes' least gap since the last model pass (zero: as fast as `gap_ms`).
    slow_ms: AtomicU64,
    quiet_ms: AtomicU64,
    /// The loop's tick: a lane opens up to half a tick early, so it is served on the tick nearest its gap, not the one
    /// after (review, night 1027: 15 and 30 min lanes ran at ~20 and ~35).
    period_ms: AtomicU64,
    /// `refresh_plan` asked since the last pass: the fast lane.
    requested: AtomicBool,
    /// The user's words as last seen, and when they last changed (`None`: not since this loop started).
    owner: Mutex<(Option<u64>, Option<std::time::Instant>)>,
    /// A change waiting for its lane, as last looked at (the session is not read again while nothing moves).
    deferred: Mutex<Option<Seen>>,
}

impl Shared {
    /// The slow lane is open: its gap since the last model pass passed (the quiet lane's once the user is quiet).
    fn lane_open(&self) -> bool {
        let quiet = self.owner.lock().unwrap().1.is_none_or(|at| at.elapsed() >= QUIET_AFTER);
        let lane = Duration::from_millis(if quiet { &self.quiet_ms } else { &self.slow_ms }.load(Ordering::Acquire));
        let slack = Duration::from_millis(self.period_ms.load(Ordering::Acquire) / 2);
        self.last_call.lock().unwrap().is_none_or(|at| at.elapsed() + slack >= lane)
    }
    /// Records the user's words as seen now; true when they changed since the last look (the first look sets the baseline).
    fn saw_owner(&self, hash: u64) -> bool {
        let mut t = self.owner.lock().unwrap();
        let changed = t.0.is_some_and(|h| h != hash);
        if changed { t.1 = Some(std::time::Instant::now()) }
        t.0 = Some(hash);
        changed
    }
    /// How long until the next model pass may start (zero: now).
    fn wait(&self) -> Duration {
        let gap = Duration::from_millis(self.gap_ms.load(Ordering::Acquire));
        self.last_call.lock().unwrap().map_or(Duration::ZERO, |at| gap.saturating_sub(at.elapsed()))
    }
}

/// The agent's handle: `refresh_plan` and the loop's lifetime.
#[derive(Clone, Default)]
pub struct Refresher {
    shared: Arc<Shared>,
}

pub type Deliver = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

impl Refresher {
    /// The `refresh_plan` tool: returns at once. A pass already running absorbs it into one follow-up pass; within
    /// `gap` of the last model pass it runs when the gap ends.
    pub fn request(&self) -> String {
        if self.shared.attached.load(Ordering::Acquire) == 0 {
            return "The plan refresher is not running; the lists stay as they are.".into();
        }
        // A plan island the user closed shows again now, not after the gap (bug 2026-10-23T23-46-23).
        let islands = self.shared.islands.lock().unwrap().clone();
        if let Some(Err(e)) = islands.map(|r| lists::reopen_closed(&r)) { eprintln!("Plan refresher: closed lists not reopened: {e:#}") }
        // Set before the wake: a loop woken in between would run the pass without the request (review, night 1027).
        self.shared.requested.store(true, Ordering::Release);
        self.shared.wake.notify_one();
        let wait = self.shared.wait();
        if wait.is_zero() { return STARTED.into() }
        let min = wait.as_secs().div_ceil(60).max(1);
        format!("The plan refreshes in about {min} min (one pass every 5 minutes at most).")
    }

    /// The loop: every `period` and on each request, until `stop` turns true, with model passes at least `gap`
    /// apart (requests meanwhile coalesce into one pass when the gap ends). `deliver` hands the diff notice to the
    /// agent.
    pub fn spawn(&self, cfg: Config, deliver: Deliver, period: Duration, gap: Duration, mut stop: tokio::sync::watch::Receiver<bool>) -> tokio::task::JoinHandle<()> {
        let shared = self.shared.clone();
        shared.gap_ms.store(gap.as_millis() as u64, Ordering::Release);
        shared.slow_ms.store(gap.as_millis() as u64 * SLOW_LANES as u64, Ordering::Release);
        shared.quiet_ms.store(gap.as_millis() as u64 * QUIET_LANES as u64, Ordering::Release);
        shared.period_ms.store(period.as_millis() as u64, Ordering::Release);
        *shared.islands.lock().unwrap() = Some(cfg.islands.clone());
        shared.attached.fetch_add(1, Ordering::AcqRel);
        // Counted down however the loop ends: stopped, or dropped with its runtime.
        struct Alive(Arc<Shared>);
        impl Drop for Alive {
            fn drop(&mut self) { self.0.attached.fetch_sub(1, Ordering::AcqRel); }
        }
        let alive = Alive(shared.clone());
        tokio::spawn(async move {
            let _alive = alive;
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                if *stop.borrow() { break }
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shared.wake.notified() => {}
                    _ = crate::background_gate::resumed() => {}
                    r = stop.changed() => { if r.is_err() || *stop.borrow() { break } else { continue } }
                }
                // Too soon after the last model pass: wait out the gap (stop still ends the loop at once).
                let ready = tokio::time::Instant::now() + shared.wait();
                let mut stopped = false;
                while tokio::time::Instant::now() < ready {
                    tokio::select! {
                        _ = tokio::time::sleep_until(ready) => {}
                        r = stop.changed() => { if r.is_err() || *stop.borrow() { stopped = true; break } }
                    }
                }
                if stopped { break }
                // Requests that arrived while waiting are served by this pass.
                tokio::select! { biased; _ = shared.wake.notified() => {} _ = std::future::ready(()) => {} }
                match pass(&cfg, &shared).await {
                    Ok(Some(text)) => {
                        let deliver = deliver.clone();
                        let sent = tokio::task::spawn_blocking(move || deliver(&text)).await;
                        if let Err(e) = sent.map_err(anyhow::Error::from).and_then(|r| r) {
                            eprintln!("Plan refresher: notice not delivered: {e:#}");
                        }
                    }
                    Ok(None) => {}
                    // Agent off or emergency mode's pause reached the request: a skip, not a failure.
                    Err(e) if crate::background_gate::quiet(&e) => {}
                    Err(e) => eprintln!("Plan refresher: pass failed: {e:#}"),
                }
            }
        })
    }
}

/// Background Tasks ▸ Plan Lists' own model when one is set, else the shared Background Tasks ▸ Model (review, 30
/// Oct: the loop asked `endpoint()` and ignored a per-task choice).
pub fn live_endpoint() -> Endpoint { crate::background_model::task_endpoint(crate::background_model::Task::Plan) }

/// The installed loop: the agent's own outbox for the notice, the islands host's folder (a lab Heimdall's own, never
/// the user's). No islands host: nothing runs. (`_workers`: bound threads are the binding's and the island agent's now.)
pub fn spawn_live(
    refresher: &Refresher,
    data: &Path,
    actions: Arc<agent::actions::Actions>,
    _workers: Arc<t3::Workers>,
    gate: Arc<agent::delivery::DeliveryGate>,
    stop: tokio::sync::watch::Receiver<bool>,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(host) = crate::islands::host() else {
        eprintln!("Plan refresher: Floating Islands are not running in this Heimdall; the plan is not refreshed.");
        return None;
    };
    let enabled_gate = gate.clone();
    let cfg = Config {
        data: data.into(),
        islands: host.root.clone(),
        // The night pass's notes in the home folder; the public edition has none (an absent folder reads as no note).
        notes: if crate::edition::PUBLIC { data.join("notes") } else { heimdall_home::home_dir().join("Assistant") },
        endpoint: Arc::new(live_endpoint),
        // Background Tasks ▸ Plan Lists off: no pass.
        enabled: Arc::new(move || crate::background_model::enabled(crate::background_model::Task::Plan) && enabled_gate.check().is_ok()),
    };
    let deliver: Deliver = Arc::new(move |text: &str| {
        let permit = gate.permit()?;
        actions.enqueue(&format!("{NOTICE_PREFIX}{}", agent::now_ms()), text, &permit)?;
        actions.drain(&permit)
    });
    Some(refresher.spawn(cfg, deliver, PERIOD, PERIOD, stop))
}

// ---------------------------------------------------------------------------------------------------------
// One pass.

static NO_KEY_LOGGED: AtomicBool = AtomicBool::new(false);

/// One pass: `Ok(Some(notice))` when the plan changed, `Ok(None)` when skipped or unchanged.
async fn pass(cfg: &Config, shared: &Shared) -> Result<Option<String>> {
    if !(cfg.enabled)() { return Ok(None) }
    // Emergency mode's pause: nothing is read or sent; the loop wakes on resume (`background_gate::resumed`).
    if crate::background_gate::emergency_paused().is_some() { return Ok(None) }
    // No key (none stored, or Background Tasks ▸ Model off Anthropic): nothing is read, no call.
    // Off the runtime's worker: resolving a ChatGPT sign-in may block (Keychain, a token refresh), and a nested
    // runtime inside a tokio worker panics (review 27 Oct).
    let resolve = cfg.endpoint.clone();
    let endpoint = tokio::task::spawn_blocking(move || resolve()).await?;
    if endpoint.key.is_none() {
        // Background Tasks ▸ Model off Anthropic: `background_model` logged the skip already.
        if crate::background_model::current().provider == agent::provider::ANTHROPIC && !NO_KEY_LOGGED.swap(true, Ordering::AcqRel) {
            eprintln!("Plan refresher: no Anthropic API key stored; the lists are not refreshed (connect Anthropic to turn it on).");
        }
        return Ok(None);
    }
    let last = shared.last.lock().unwrap().clone();
    let requested = shared.requested.swap(false, Ordering::AcqRel);
    let deferred = shared.deferred.lock().unwrap().clone();
    let look = |base: Option<Seen>| {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || gather(&cfg, base.as_ref()))
    };
    // Nothing new since the last look (the session file, the note, the islands and threads as seen then). A change
    // already waiting for its lane is read again only once its lane is open.
    let gathered = match look(deferred.clone().or_else(|| last.clone())).await?? {
        Some(g) => g,
        None if deferred.is_some() && (requested || shared.lane_open()) => match look(None).await?? { Some(g) => g, None => return Ok(None) },
        None => return Ok(None),
    };
    shared.reads.fetch_add(1, Ordering::AcqRel);
    let owner_spoke = shared.saw_owner(gathered.fingerprint.owner);
    if last.as_ref().is_some_and(|l| l.fp == gathered.fingerprint) {
        // Only bookkeeping or the refresher's own notice was added: remember the file as it is now.
        *shared.last.lock().unwrap() = Some(Seen { fp: gathered.fingerprint, stat: gathered.stat });
        *shared.deferred.lock().unwrap() = None;
        return Ok(None);
    }
    // The user's words or a `refresh_plan` request: what this pass removes may be what he asked for.
    let by_owner = requested || owner_spoke || last.as_ref().is_some_and(|l| l.fp.owner != gathered.fingerprint.owner);
    let fast = by_owner || last.is_none();
    if !fast && !shared.lane_open() {
        *shared.deferred.lock().unwrap() = Some(Seen { fp: gathered.fingerprint, stat: gathered.stat.clone() });
        return Ok(None);
    }
    *shared.deferred.lock().unwrap() = None;
    eprintln!("Plan refresher: pass for {}", changed_parts(last.as_ref().map(|l| &l.fp), &gathered.fingerprint, requested));
    let prompt = user_prompt(&gathered);
    shared.calls.fetch_add(1, Ordering::AcqRel);
    *shared.last_call.lock().unwrap() = Some(std::time::Instant::now());
    // A failed call (network, timeout, server error) leaves the fingerprint alone: the next tick tries again.
    let answer = match call_model(&endpoint, &prompt).await {
        Ok(a) => a.text,
        // Over the $0.50 limit (logged): the same input would be skipped again, so wait for something new.
        Err(e) if e.downcast_ref::<crate::background_call::OverLimit>().is_some() => {
            *shared.last.lock().unwrap() = Some(Seen { fp: gathered.fingerprint, stat: gathered.stat });
            return Ok(None);
        }
        // Paid for without an answer (cut off, at the limit, refused, timed out): the same input is not sent again.
        Err(e) if crate::background_call::settled(&e) => {
            *shared.last.lock().unwrap() = Some(Seen { fp: gathered.fingerprint, stat: gathered.stat });
            return Err(e);
        }
        // Network or server errors cost nothing: the next tick tries again, a request still in the fast lane.
        Err(e) => {
            if requested { shared.requested.store(true, Ordering::Release) }
            return Err(e);
        }
    };
    let plan = match validate(&answer, &gathered.lists, &gathered.jobs, &gathered.pictures, &gathered.conversation.join("\n")) {
        Ok(plan) => plan,
        Err(e) => {
            // The same input would give the same bad answer: wait for something new.
            *shared.last.lock().unwrap() = Some(Seen { fp: gathered.fingerprint, stat: gathered.stat });
            bail!("model answer rejected: {e:#}");
        }
    };
    // Since 2 Nov a pass only proposes (`apply`): nothing it drops leaves Scheduled, so nothing young needs holding.
    let diff = apply(&cfg.islands, &gathered.lists, &plan, &gathered.jobs)?;
    let after = Fingerprint { islands: islands_hash(&cfg.islands.all()), ..gathered.fingerprint };
    *shared.last.lock().unwrap() = Some(Seen { fp: after, stat: gathered.stat });
    Ok(diff_text(&diff))
}

/// A plan item as stored (the full object, unknown keys kept), by list (`LISTS` order).
type Lists = [Vec<Map<String, Value>>; 2];

/// A job island as on disk (every key), and the thread it follows.
#[derive(Clone, Debug, PartialEq)]
struct Job {
    id: String,
    thread: Option<String>,
    island: Map<String, Value>,
}

impl Job {
    fn s(&self, k: &str) -> &str { self.island.get(k).and_then(Value::as_str).unwrap_or("") }
}

struct Gathered {
    fingerprint: Fingerprint,
    stat: SessionStat,
    lists: Lists,
    jobs: Vec<Job>,
    summary: Option<String>,
    conversation: Vec<String>,
    /// Pictures named in the recent conversation that are real image files, newest first.
    pictures: Vec<String>,
    note: Option<(String, String)>,
}

/// What a pass reads. `None` when the session file (by size and mtime), the night note and the islands are as `last`
/// saw them: the session is not read then.
fn gather(cfg: &Config, last: Option<&Seen>) -> Result<Option<Gathered>> {
    // First contact with the user's old-shape lists: backed up once, then migrated in place (ids for string items).
    let migrated = lists::migrate_all(&cfg.islands)?;
    let reopened = lists::reopen_closed(&cfg.islands)?;
    if !reopened.is_empty() { eprintln!("Plan refresher: showed the closed lists {reopened:?} again"); }
    if !migrated.is_empty() { eprintln!("Plan refresher: migrated the plan lists {migrated:?} to the night-1022 shape"); }
    let session = session_path(&cfg.data);
    let meta = std::fs::metadata(&session).ok();
    let session_len = meta.as_ref().map_or(0, |m| m.len());
    let stat = SessionStat(session.clone(), session_len, meta.and_then(|m| m.modified().ok()));
    let note_path = newest_note(&cfg.notes);
    let note_meta = note_path.as_ref().and_then(|p| std::fs::metadata(p).ok());
    let note_stamp = {
        let mut h = DefaultHasher::new();
        note_path.hash(&mut h);
        note_meta.as_ref().map(|m| (m.len(), m.modified().ok())).hash(&mut h);
        h.finish()
    };
    let all = cfg.islands.all();
    let islands = islands_hash(&all);
    if last.is_some_and(|l| l.stat == stat && l.fp.note == note_stamp && l.fp.islands == islands) {
        return Ok(None);
    }
    let (lists, jobs) = plan_of(&all);
    let (summary, conversation, newest_owner) = read_session(&session, session_len).unwrap_or_else(|e| {
        eprintln!("Plan refresher: session unreadable: {e:#}");
        (None, Vec::new(), None)
    });
    let said = {
        let mut h = DefaultHasher::new();
        summary.hash(&mut h);
        conversation.hash(&mut h);
        h.finish()
    };
    let owner = {
        let mut h = DefaultHasher::new();
        newest_owner.hash(&mut h);
        h.finish()
    };
    let fingerprint = Fingerprint { session: said, owner, note: note_stamp, islands };
    let pictures = pictures_in(conversation.iter().rev().map(String::as_str));
    let note = note_path.and_then(|p| {
        let text = std::fs::read_to_string(&p).ok()?;
        Some((p.file_name()?.to_string_lossy().into_owned(), clip(&text, NOTE_BUDGET)))
    });
    Ok(Some(Gathered { fingerprint, stat, lists, jobs, summary, conversation, pictures, note }))
}

/// A conversation line carrying the user's words: an incoming delivery that is not a notice (`[Heimdall delivery:
/// notice-…]`: T3, island and wake notices are bookkeeping, however often they arrive).
fn owners_line(l: &str) -> bool {
    l.strip_prefix("Incoming: ").is_some_and(|t| !t.starts_with("[Heimdall delivery: notice-"))
}

/// What moved since the last model pass, for its log line (night 1027: why a pass ran was never logged).
fn changed_parts(last: Option<&Fingerprint>, now: &Fingerprint, requested: bool) -> String {
    let Some(l) = last else { return "the first look since start".into() };
    let mut parts = Vec::new();
    if requested { parts.push("refresh_plan") }
    if l.owner != now.owner { parts.push("User's words") } else if l.session != now.session { parts.push("notices or Heimdall's lines") }
    if l.note != now.note { parts.push("the night note") }
    if l.islands != now.islands { parts.push("islands") }
    if parts.is_empty() { "nothing".into() } else { parts.join(", ") }
}


pub use crate::pi_session::session_path;

/// The newest `night-pass-*.md` by name (names carry the date).
fn newest_note(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("night-pass-") && n.ends_with(".md")))
        .max()
}

/// The two lists and the job islands from what is on disk, bound ones first. Plan islands are known by id; their
/// items are read like everywhere else (`islands::plan::items`: a plain string is an item, every item has an id).
/// A job island is any other island that is not a viewer, a note, a list or hidden (`todo` and `goals` are lists,
/// `preset: "todo"`, not this job's: skipped, as in `islands_hash`).
fn plan_of(all: &[(String, Map<String, Value>)]) -> (Lists, Vec<Job>) {
    let mut lists: Lists = Default::default();
    let mut jobs = Vec::new();
    for (id, o) in all {
        if let Some(i) = LISTS.iter().position(|(l, _)| l == id) {
            lists[i] = lists::items(o);
            continue;
        }
        if !is_job(o) { continue }
        let thread = match o.get("thread") {
            Some(Value::Object(t)) => t.get("id").and_then(Value::as_str).map(str::to_owned),
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        jobs.push(Job { id: id.clone(), thread: thread.filter(|t| !t.trim().is_empty()), island: o.clone() });
    }
    jobs.sort_by_key(|j| j.thread.is_none());
    (lists, jobs)
}

/// The island follows a T3 thread (`"thread": {"id": …}` or a plain id).
fn bound(o: &Map<String, Value>) -> bool {
    match o.get("thread") {
        Some(Value::Object(t)) => t.get("id").and_then(Value::as_str).is_some_and(|t| !t.trim().is_empty()),
        Some(Value::String(t)) => !t.trim().is_empty(),
        _ => false,
    }
}

fn is_job(o: &Map<String, Value>) -> bool {
    let preset = o.get("preset").and_then(Value::as_str).unwrap_or("task");
    matches!(preset, "task" | "waiting" | "result") && !o.contains_key("path") && o.get("hidden") != Some(&Value::Bool(true))
}

/// What a job island shows that the refresher reads and writes.
const JOB_KEYS: [&str; 11] = ["thread", "title", "state", "stage", "summary", "steps", "progress", "kv", "image", "open", "parts"];

/// What decides "something new" on the islands side: the lists' items, what an unbound job island shows, and a bound
/// one's title and state (code rewrites its stage, history and counters every minute; that is not plan news).
fn islands_hash(all: &[(String, Map<String, Value>)]) -> u64 {
    let mut h = DefaultHasher::new();
    for (id, o) in all {
        if LISTS[..SETTLED].iter().any(|(l, _)| l == id) {
            id.hash(&mut h);
            o.get("items").map(|v| v.to_string()).hash(&mut h);
        } else if is_job(o) && bound(o) {
            id.hash(&mut h);
            for k in ["title", "state"] { o.get(k).map(|v| v.to_string()).hash(&mut h); }
        } else if is_job(o) {
            id.hash(&mut h);
            for k in JOB_KEYS { o.get(k).map(|v| v.to_string()).hash(&mut h); }
        }
    }
    h.finish()
}

/// The agent's current session as plain lines, oldest first, with the latest compaction summary (whole): what Pi
/// sends the model (`buildContextEntries`), read from the file as it is.
fn read_session(path: &Path, len: u64) -> Result<(Option<String>, Vec<String>, Option<String>)> {
    if len == 0 { return Ok((None, Vec::new(), None)) }
    let entries = crate::pi_session::entries(path, len)?;
    let newest_owner = newest_owner(&entries);
    let (summary, context) = active_context(&entries);
    let mut out: Vec<String> = context.iter().flat_map(|e| render_entry(e)).collect();
    let mut used: usize = out.iter().map(|t| t.len() + 1).sum();
    let mut cut = 0;
    // `TRIM_LINES` at a time, so the chunks that stay are the ones the last passes sent.
    while used > CONVERSATION_CEILING && cut < out.len() {
        let next = (cut + TRIM_LINES).min(out.len());
        used -= out[cut..next].iter().map(|t| t.len() + 1).sum::<usize>();
        cut = next;
    }
    out.drain(..cut);
    Ok((summary, out, newest_owner))
}

/// The user's newest line in the whole file, not the window: its entry id, else timestamp, else its text and how many
/// entries of his the file holds. A compaction or a trim of older lines leaves it as it is (review, night 1027:
/// hashing every the user line in the window ran a fast pass and reset the quiet clock on each compaction).
fn newest_owner(entries: &[Value]) -> Option<String> {
    let said = |e: &Value| e["message"]["role"] == "user" && render_entry(e).iter().any(|l| owners_line(l));
    let newest = entries.iter().rev().find(|e| said(e))?;
    if let Some(key) = newest["id"].as_str().or(newest["timestamp"].as_str()) { return Some(key.to_owned()) }
    Some(format!("{} {}", entries.iter().filter(|e| said(e)).count(), render_entry(newest).concat()))
}

// ---------------------------------------------------------------------------------------------------------
// The model.

/// Haiku's instructions (everything he said about islands, with worked examples; a long prompt
/// is fine for a cheap model). The examples are their own file so they are easy to extend.
/// The public edition (docs/editions.md) has its own copy: no the user, no quotes of his, no private listings or paths.
#[cfg(not(feature = "public"))]
pub const SYSTEM: &str = concat!(include_str!("../prompt/plan-refresher.md"), include_str!("../prompt/plan-refresher-examples.md"));
#[cfg(feature = "public")]
pub const SYSTEM: &str = concat!(include_str!("../prompt/public/plan-refresher.md"), include_str!("../prompt/public/plan-refresher-examples.md"));

/// The answer's JSON schema (structured outputs: no length or count constraints; the caps are enforced here).
/// Anthropic compiles the schema into one grammar and refuses a large one ("The compiled grammar is too large", every
/// pass on 22 Oct). The lists share one item definition through `$ref` (written out three times, the schema failed;
/// measured against the API, see `lab/refresher-grammar/`), and every key is required, empty meaning none: each
/// optional key roughly doubles part of the grammar. No `todo`: To do is Goals' (31 Oct).
pub fn schema() -> Value {
    let item = json!({
        "type": "object",
        "properties": {
            // Three short descriptions repeat the row rules where the model writes them (the grammar test caps the bytes).
            "id": {"type": "string"},
            "title": {"type": "string", "description": "At most 6 words, project first."},
            "done": {"type": "boolean"},
            "when": {"type": "string", "description": "Scheduled: 'YYYY-MM-DDTHH:MM', or 'Later'."},
            "detail": {"type": "string", "description": "Line 1: at most 8 words, never 'Needs:'. Optional line 2: one sentence. Rewrite one that breaks this."}
        },
        "required": ["id", "title", "done", "when", "detail"],
        "additionalProperties": false
    });
    let step = json!({
        "type": "object",
        "properties": {"title": {"type": "string"}, "state": {"type": "string", "enum": ["done", "now", "failed", "todo"]}},
        "required": ["title", "state"],
        "additionalProperties": false
    });
    let part = json!({
        "type": "object",
        "properties": {
            "type": {"type": "string", "enum": ["stat", "bars", "line"]},
            "label": {"type": "string"},
            "value": {"type": "number"},
            "unit": {"type": "string"},
            "items": {"type": "array", "items": {
                "type": "object",
                "properties": {"label": {"type": "string"}, "value": {"type": "number"}},
                "required": ["label", "value"],
                "additionalProperties": false
            }},
            "points": {"type": "array", "items": {"type": "number"}}
        },
        "required": ["type", "label", "value", "unit", "items", "points"],
        "additionalProperties": false
    });
    let fact = json!({
        "type": "object",
        "properties": {"label": {"type": "string"}, "value": {"type": "string"}},
        "required": ["label", "value"],
        "additionalProperties": false
    });
    // Every key required (empty values mean none or keep): no unions for the grammar.
    let job = json!({
        "type": "object",
        "properties": {
            "island": {"type": "string"},
            "title": {"type": "string"},
            "state": {"type": "string", "enum": ["running", "waiting", "done", "failed"]},
            "stage": {"type": "string"},
            "summary": {"type": "string"},
            "steps": {"type": "array", "items": step},
            "progress": {
                "type": "object",
                "properties": {"done": {"type": "number"}, "total": {"type": "number"}, "unit": {"type": "string"}},
                "required": ["done", "total", "unit"],
                "additionalProperties": false
            },
            "parts": {"type": "array", "items": part},
            "icon": {"type": "string"},
            "facts": {"type": "array", "items": fact},
            "image": {"type": "string"},
            "open": {"type": "string"},
            "clear": {"type": "array", "items": {"type": "string", "enum": ["steps", "facts", "image", "icon"]}}
        },
        "required": ["island", "title", "state", "stage", "summary", "steps", "progress", "parts", "facts", "image", "open", "icon", "clear"],
        "additionalProperties": false
    });
    json!({
        "type": "object",
        "properties": {
            "scheduled": {"type": "array", "items": {"$ref": "#/$defs/item"}},
            "jobs": {"type": "array", "items": job}
        },
        "required": ["scheduled", "jobs"],
        "additionalProperties": false,
        "$defs": {"item": item}
    })
}

/// The run on the background model (`background_call`: adaptive thinking with `output_config.effort`, structured
/// outputs through `output_config.format`, the one-hour prompt cache, the $0.50 limit).
pub fn call<'a>(model: &str, system: &'a str, prompt: crate::background_call::Prompt) -> crate::background_call::Call<'a> {
    crate::background_call::Call { job: JOB, model: model.to_owned(), system, schema: schema(), effort: EFFORT, prompt, history: Vec::new(), session: None }
}

/// The Messages API body a pass sends (tests, the replay eval).
pub fn request_body(prompt: impl Into<crate::background_call::Prompt>) -> Value {
    request_body_for(&crate::background_model::anthropic_model(), SYSTEM, prompt)
}

/// `request_body` naming `model` (the endpoint's snapshot) and the system prompt (the replay eval compares prompts).
pub fn request_body_for(model: &str, system: &str, prompt: impl Into<crate::background_call::Prompt>) -> Value {
    crate::background_call::prepare(&call(model, system, prompt.into())).map(|p| p.body).unwrap_or(Value::Null)
}

fn user_prompt(g: &Gathered) -> crate::background_call::Prompt { user_prompt_at(g, chrono::Local::now()) }

/// The prompt as a pass at `now` sends it (the replay eval sets the recording's clock). Stable first, for the prompt
/// cache: the compaction summary, then the conversation in `CHUNK_LINES` chunks (each complete chunk and the newest
/// line end a cached stretch). Then what moves between passes: the night note, the unbound job islands, the bound ones
/// as read-only lines, the lists, and last the clock and the instruction.
fn user_prompt_at(g: &Gathered, now: chrono::DateTime<chrono::Local>) -> crate::background_call::Prompt {
    let mut prompt = crate::background_call::Prompt::default();
    if let Some(s) = &g.summary {
        prompt.stable(format!("<earlier_conversation_summary>\n{s}\n</earlier_conversation_summary>\n\n"));
    }
    prompt.stable("<recent_conversation note=\"oldest first; Incoming lines are User's words, transcripts and notices\">\n");
    for chunk in g.conversation.chunks(CHUNK_LINES) {
        let mut t = String::new();
        for line in chunk { t.push_str(line); t.push('\n'); }
        prompt.stable(t);
    }
    let mut p = String::from("</recent_conversation>\n\n");
    if let Some((name, text)) = &g.note {
        p.push_str(&format!("<night_pass_note file=\"{name}\">\n{text}\n</night_pass_note>\n\n"));
    }
    let jobs: Vec<Value> = g.jobs.iter().filter(|j| j.thread.is_none()).take(MAX_JOBS).map(|j| {
        let mut v = json!({"island": j.id, "title": j.s("title"), "state": j.s("state"),
                           "stage": j.s("stage"), "summary": j.s("summary")});
        for k in ["icon", "steps", "kv", "progress", "image", "open"] {
            if let Some(x) = j.island.get(k) { v[if k == "kv" { "facts" } else { k }] = x.clone(); }
        }
        let own: Vec<&Value> = j.island.get("parts").and_then(Value::as_array).into_iter().flatten().filter(|p| p.get(OWN_PART).is_some()).collect();
        if !own.is_empty() { v["parts"] = json!(own); }
        if j.s("attention") == "question" { v["open_question_for_owner"] = json!(true); }
        v
    }).collect();
    if !jobs.is_empty() {
        p.push_str(&format!("<job_islands>\n{}\n</job_islands>\n\n", serde_json::to_string_pretty(&jobs).unwrap_or_default()));
        if !g.pictures.is_empty() {
            p.push_str(&format!("<pictures_in_conversation note=\"newest first\">\n{}\n</pictures_in_conversation>\n\n", g.pictures.join("\n")));
        }
    }
    // Bound islands: code and the island agent keep them; one line each, so the pass knows that work is shown.
    let bound: Vec<String> = g.jobs.iter().filter(|j| j.thread.is_some()).take(MAX_JOBS)
        .map(|j| format!("- {} · {} · {} · {}", j.id, j.s("title"), if j.s("state").is_empty() { "running" } else { j.s("state") }, j.s("stage"))).collect();
    if !bound.is_empty() {
        p.push_str(&format!("<thread_islands note=\"read-only: id · title · state · stage; never in jobs\">\n{}\n</thread_islands>\n\n", bound.join("\n")));
    }
    let view = |items: &Vec<Map<String, Value>>, scheduled: bool| -> Vec<Value> {
        items.iter().map(|o| {
            let mut v = Map::new();
            for k in ["id", "title", "done", "when", "detail"] {
                if let Some(x) = o.get(k) { v.insert(k.into(), x.clone()); }
            }
            // A Scheduled item's due time reaches the model the way it writes one: a local date-time in `when`.
            if let Some(due) = o.get("due").and_then(Value::as_f64).filter(|_| scheduled).and_then(|d| chrono::DateTime::from_timestamp(d as i64, 0)) {
                v.insert("when".into(), json!(due.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M").to_string()));
            }
            Value::Object(v)
        }).collect()
    };
    let current = json!({"scheduled": view(&g.lists[0], true)});
    p.push_str(&format!("<current_lists>\n{}\n</current_lists>\n\n", serde_json::to_string_pretty(&current).unwrap_or_default()));
    p.push_str(&format!("Now: {}\n\nRewrite the lists and rebuild the job islands now.", now.format("%A %Y-%m-%d %H:%M")));
    prompt.push(p);
    prompt
}

/// One run on the background model; returns the answer with its usage. Errors never carry the key.
async fn call_model(e: &Endpoint, prompt: &crate::background_call::Prompt) -> Result<crate::background_call::Answer> { call_model_with(e, SYSTEM, prompt).await }

async fn call_model_with(e: &Endpoint, system: &str, prompt: &crate::background_call::Prompt) -> Result<crate::background_call::Answer> {
    let model = e.model.clone().unwrap_or_else(crate::background_model::anthropic_model);
    crate::background_call::ask(e, &call(&model, system, prompt.clone())).await
}

// ---------------------------------------------------------------------------------------------------------
// Validation, applying and the diff.

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Empty: new.
    pub id: String,
    pub title: String,
    pub done: bool,
    pub when: Option<String>,
    pub detail: Option<String>,
}

/// One job island as the model rebuilt it, checked. `None` keeps what the island shows now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct JobUpdate {
    pub island: String,
    pub title: Option<String>,
    /// `None` over an open question (the thread binding owns that state).
    pub state: Option<String>,
    pub stage: Option<String>,
    pub summary: Option<String>,
    /// (title, done/now/failed/todo).
    pub steps: Option<Vec<(String, String)>>,
    /// (done, total, unit); `None` clears only a progress the refresher wrote.
    pub progress: Option<(f64, f64, String)>,
    /// The refresher's own display parts (stat, bars, line) as the app draws them, replaced each pass.
    pub parts: Vec<Value>,
    /// A drawable icon (`islands::icon`): an SF Symbol name or a tiny SVG.
    pub icon: Option<String>,
    pub facts: Option<Vec<(String, String)>>,
    pub image: Option<String>,
    pub open: Option<String>,
    /// Keys to remove because they no longer apply (`steps`, `kv`, `image`).
    pub clear: Vec<&'static str>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Scheduled and Settled (`LISTS` order).
    pub lists: [Vec<Item>; 2],
    pub jobs: Vec<JobUpdate>,
    /// Items the model dropped that stay where they are (`hold_young`): not the model's to remove this pass.
    pub keep: Vec<String>,
}

/// The model's answer, checked: well-formed JSON of the schema, ids known or new, strings and lists capped, done
/// items sunk. A malformed answer is an error (nothing is written); so is one that would empty a non-empty plan.
/// `pictures` and `conversation` are what the recent conversation shows (islands without a thread draw on them). Only
/// unbound job islands are rebuilt. A `todo` key in the answer is ignored: To do is Goals' (31 Oct).
fn validate(answer: &str, current: &Lists, jobs: &[Job], pictures: &[String], conversation: &str) -> Result<Plan> {
    let v: Value = serde_json::from_str(answer.trim()).context("not JSON")?;
    let o = v.as_object().context("not a JSON object")?;
    // Settled's ids are not the model's (review 2 Nov: one returned under scheduled was copied out of Settled).
    let known: HashSet<&str> = current[..SETTLED].iter().flatten().filter_map(|o| o.get("id").and_then(Value::as_str)).collect();
    let mut seen = HashSet::new();
    let mut lists: [Vec<Item>; 2] = Default::default();
    for (i, (name, _)) in LISTS.iter().enumerate() {
        if i == SETTLED { continue }
        let arr = o.get(*name).and_then(Value::as_array).with_context(|| format!("{name}: missing list"))?;
        for (n, raw) in arr.iter().enumerate() {
            let it = raw.as_object().with_context(|| format!("{name}[{n}]: not an object"))?;
            let s = |k: &str| -> Result<Option<String>> {
                match it.get(k) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(s)) => Ok(Some(s.trim().to_owned())),
                    _ => bail!("{name}[{n}].{k}: not a string"),
                }
            };
            let title = s("title")?.filter(|t| !t.is_empty()).with_context(|| format!("{name}[{n}]: no title"))?;
            let done = match it.get("done") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(b)) => *b,
                _ => bail!("{name}[{n}].done: not true/false"),
            };
            let mut id = s("id")?.unwrap_or_default();
            // An unknown or repeated id is a new item (the model cannot mint ids).
            if !known.contains(id.as_str()) || !seen.insert(id.clone()) { id = String::new(); }
            lists[i].push(Item {
                id,
                title: clip(&title, TITLE_CHARS),
                done,
                when: s("when")?.filter(|w| !w.is_empty()).map(|w| clip(&w, WHEN_CHARS)),
                detail: s("detail")?.filter(|d| !d.is_empty()).map(|d| clip(&quiet_detail(&d), DETAIL_CHARS)).filter(|d| !d.is_empty()),
            });
        }
        // Ticked items sink to the bottom.
        lists[i].sort_by_key(|it| it.done);
        if lists[i].len() > LIST_CAP[i] {
            eprintln!("Plan refresher: {name} capped at {} items (answer had {})", LIST_CAP[i], lists[i].len());
            // Keep open items first; drop the oldest done ones from the end.
            lists[i].truncate(LIST_CAP[i]);
        }
    }
    let mut out_jobs: Vec<JobUpdate> = Vec::new();
    for j in o.get("jobs").and_then(Value::as_array).into_iter().flatten() {
        let island = j["island"].as_str().context("jobs: an entry without island")?;
        // Only unbound islands the pass showed: a bound one is code's and the island agent's.
        let Some(job) = jobs.iter().filter(|x| x.thread.is_none()).take(MAX_JOBS).find(|x| x.id == island) else { continue };
        if out_jobs.iter().any(|u| u.island == island) { continue }
        out_jobs.push(job_update(j, job, pictures, conversation));
    }
    let before: usize = current[..SETTLED].iter().map(Vec::len).sum();
    let after: usize = lists[..SETTLED].iter().map(Vec::len).sum();
    ensure!(!(before >= 3 && after == 0), "the answer would empty Scheduled ({before} items now)");
    Ok(Plan { lists, jobs: out_jobs, keep: Vec::new() })
}

/// A detail without the "Needs:" the rows no longer show (the row's symbol says what it asks); its first letter upper
/// case. An old detail starting so is rewritten by the model; this catches one it wrote anyway.
pub fn quiet_detail(d: &str) -> String {
    let d = d.trim();
    let Some(rest) = d.strip_prefix("Needs:").or_else(|| d.strip_prefix("needs:")).map(str::trim_start) else { return d.to_owned() };
    let mut c = rest.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// At most `n` words.
fn words(s: &str, n: usize) -> String { s.split_whitespace().take(n).collect::<Vec<_>>().join(" ") }

/// One line of at most `n` characters, cut at a word when it is long.
fn line(s: &str, n: usize) -> String {
    let t = one_line(s);
    if t.chars().count() <= n { return t }
    let cut: String = t.chars().take(n - 1).collect();
    let cut = match cut.rfind(' ') { Some(i) if i > n / 2 => cut[..i].trim_end_matches([',', ';', ':']).to_owned(), _ => cut };
    format!("{cut}\u{2026}")
}

/// A target a click can open safely: a web URL, a plain folder, or a document or media file (its real path after
/// symlinks). Apps, bundles, scripts, installers, links and executables are refused: a click would run them.
fn openable(t: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let t = t.trim();
    if t.contains('\u{2026}') || t.contains(char::is_whitespace) && !t.starts_with('/') { return false }
    if let Some(rest) = t.strip_prefix("https://").or_else(|| t.strip_prefix("http://")) { return !rest.is_empty() && !t.contains(char::is_whitespace) }
    if !t.starts_with('/') { return false }
    let Ok(real) = std::fs::canonicalize(t) else { return false };
    let ext = real.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let Ok(m) = std::fs::metadata(&real) else { return false };
    if m.is_dir() {
        // A plain folder (a dot in its name is fine); bundles would launch or install.
        const BUNDLES: [&str; 14] = ["app", "bundle", "framework", "plugin", "prefpane", "kext", "appex", "xpc", "saver", "qlgenerator",
            "mdimporter", "workflow", "action", "scptd"];
        return !BUNDLES.contains(&ext.as_str());
    }
    const DOCS: [&str; 34] = ["html", "htm", "pdf", "png", "jpg", "jpeg", "gif", "webp", "heic", "tif", "tiff", "svg", "bmp",
        "mp4", "mov", "m4v", "webm", "mkv", "mp3", "m4a", "wav", "aif", "aiff", "flac", "ogg", "md", "txt", "json", "csv", "log",
        "rtf", "docx", "xlsx", "pptx"];
    DOCS.contains(&ext.as_str()) && m.permissions().mode() & 0o111 == 0
}

/// `target` appears in `text` as a whole token (not as the start of a longer path or URL).
fn named_in(text: &str, target: &str) -> bool {
    if target.is_empty() { return false }
    let mut from = 0;
    while let Some(i) = text[from..].find(target) {
        let (start, end) = (from + i, from + i + target.len());
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        let edge = |c: Option<char>| c.is_none_or(|c| c.is_whitespace() || "\"'`()[]<>,;*:=!?".contains(c));
        let after_ok = edge(after) || (after == Some('.') && edge(text[end + 1..].chars().next()));
        if edge(before) && after_ok { return true }
        from = end;
    }
    false
}

/// A number as the island shows it: whole numbers plain, others to two places at most.
fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e12 { return format!("{}", v as i64) }
    let t = format!("{v:.2}");
    t.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// One display part from the answer, checked, in the app's shape (`islands/Sources/FloatingIslands/Parts.swift`),
/// marked as the refresher's own. `None` when it shows nothing real.
fn display_part(p: &Value) -> Option<Value> {
    let unit = clip(p["unit"].as_str().unwrap_or("").trim(), 12);
    let label = clip(&one_line(p["label"].as_str().unwrap_or("")), 24);
    match p["type"].as_str()? {
        "stat" => {
            let v = p["value"].as_f64().filter(|v| v.is_finite())?;
            let unit = [unit.as_str(), label.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" ");
            Some(json!({"type": "stat", "value": number(v), "unit": unit, OWN_PART: true}))
        }
        "bars" => {
            let items: Vec<Value> = p["items"].as_array()?.iter().filter_map(|b| {
                let l = line(b["label"].as_str()?, 24);
                let v = b["value"].as_f64().filter(|v| v.is_finite() && *v >= 0.0)?;
                (!l.is_empty()).then(|| json!({"label": l, "value": v}))
            }).take(BARS_CAP).collect();
            (!items.is_empty()).then(|| json!({"type": "bars", "items": items, "unit": unit, OWN_PART: true}))
        }
        "line" => {
            let pts: Vec<f64> = p["points"].as_array()?.iter().filter_map(Value::as_f64).filter(|v| v.is_finite()).collect();
            let pts = &pts[pts.len().saturating_sub(LINE_POINTS)..];
            (pts.len() >= 2).then(|| json!({"type": "line", "points": pts, "unit": unit, OWN_PART: true}))
        }
        _ => None,
    }
}

/// One job entry, checked against the island-style rules and what the pass knows.
fn job_update(j: &Value, job: &Job, conversation_pictures: &[String], conversation: &str) -> JobUpdate {
    let text = |k: &str| j[k].as_str().map(str::trim).unwrap_or("").to_owned();
    let mut u = JobUpdate { island: job.id.clone(), ..Default::default() };
    // The title stays unless it is missing or breaks the style rules (no rewording every pass).
    let current_title = job.s("title");
    let title_ok = |t: &str| !t.trim().is_empty() && crate::islands::tools::style_problem(&job.id, json!({"title": t}).as_object().unwrap(), false).is_none();
    let title = words(&text("title"), 4);
    if !title_ok(current_title) && title_ok(&title) { u.title = Some(clip(&title, 40)) }
    // State: the model's on an island without a thread, except over an open question.
    let state = text("state");
    if job.s("attention") != "question" && ["running", "waiting", "done", "failed"].contains(&state.as_str()) { u.state = Some(state) }
    let stage = clip(&words(&text("stage"), 2), STAGE_CHARS);
    if !stage.is_empty() { u.stage = Some(stage) }
    let summary = line(&text("summary"), SUMMARY_CHARS);
    if !summary.is_empty() { u.summary = Some(summary) }
    let mut steps: Vec<(String, String)> = j["steps"].as_array().into_iter().flatten().filter_map(|st| {
        let t = line(st["title"].as_str()?, STEP_CHARS);
        let mark = match st["state"].as_str()? { m @ ("done" | "now" | "failed" | "todo") => m.to_owned(), _ => "todo".into() };
        (!t.is_empty()).then_some((t, mark))
    }).collect();
    // The current phase fits: the oldest done steps go first.
    while steps.len() > STEPS_CAP {
        match steps.iter().position(|(_, m)| m == "done") { Some(0) => { steps.remove(0); } _ => steps.truncate(STEPS_CAP) }
    }
    // Empty keeps the current timeline.
    if !steps.is_empty() { u.steps = Some(steps) }
    let p = &j["progress"];
    if let (Some(done), Some(total)) = (p["done"].as_f64(), p["total"].as_f64()) {
        if total.is_finite() && done.is_finite() && total > 0.0 && (0.0..=total).contains(&done) {
            u.progress = Some((done, total, clip(p["unit"].as_str().unwrap_or("").trim(), 16)));
        }
    }
    u.parts = j["parts"].as_array().into_iter().flatten().filter_map(display_part).take(PARTS_CAP).collect();
    if let Some(i) = j["icon"].as_str().and_then(crate::islands::icon) { u.icon = Some(i) }
    let facts: Vec<(String, String)> = j["facts"].as_array().into_iter().flatten().filter_map(|f| {
        let (l, v) = (line(f["label"].as_str()?, 20), line(f["value"].as_str()?, 32));
        (!l.is_empty() && !v.is_empty()).then_some((l, v))
    }).take(FACTS_CAP).collect();
    // Empty keeps the current facts.
    if !facts.is_empty() { u.facts = Some(facts) }
    // A picture only when it is one the work produced (or the island already shows) and a real image file.
    let image = text("image");
    let offered = conversation_pictures.contains(&image);
    if !image.is_empty() && (offered || image == job.s("image")) && crate::islands::picture(&image) { u.image = Some(image) }
    // An open target only when the work named it (word for word) or the island has it, and a click cannot run it.
    let open = text("open");
    let named = named_in(conversation, &open) || open == job.s("open");
    if !open.is_empty() && named && openable(&open) { u.open = Some(open) }
    for c in j["clear"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let key = match c {
            "steps" if u.steps.is_none() => "steps",
            "facts" if u.facts.is_none() => "kv",
            "image" if u.image.is_none() => "image",
            "icon" if u.icon.is_none() => "icon",
            _ => continue,
        };
        if !u.clear.contains(&key) { u.clear.push(key) }
    }
    u
}

/// The island change for one rebuilt job (`null` removes a key), only what differs from `o`.
fn job_patch(o: &Map<String, Value>, u: &JobUpdate) -> Map<String, Value> {
    let mut p = Map::new();
    if let Some(t) = &u.title {
        p.insert("title".into(), json!(t));
        // The binding shows the island's title beside its thread.
        if let Some(Value::Object(th)) = o.get("thread") {
            let mut th = th.clone();
            th.insert("title".into(), json!(t));
            p.insert("thread".into(), Value::Object(th));
        }
    }
    if let Some(s) = &u.state { p.insert("state".into(), json!(s)); }
    if let Some(s) = &u.stage { p.insert("stage".into(), json!(s)); }
    if let Some(s) = &u.summary { p.insert("summary".into(), json!(s)); }
    if let Some(steps) = &u.steps {
        p.insert("steps".into(), Value::Array(steps.iter().map(|(t, m)| json!({"title": t, "state": m})).collect()));
    }
    if let Some(facts) = &u.facts {
        p.insert("kv".into(), Value::Array(facts.iter().map(|(l, v)| json!({"label": l, "value": v})).collect()));
    }
    let state = u.state.as_deref().or(o.get("state").and_then(Value::as_str)).unwrap_or("running");
    let progress = u.progress.as_ref().filter(|_| state == "running");
    match progress {
        Some((d, t, _)) => {
            let v = json!(((d / t) * 1000.0).round() / 1000.0);
            p.insert("progress".into(), v.clone());
            p.insert(OWN_PROGRESS.into(), v);
        }
        // Only the refresher's own progress goes; one the agent set since stays (and the stale mark goes).
        None if o.get(OWN_PROGRESS).is_some() => {
            if o.get(OWN_PROGRESS) == o.get("progress") { p.insert("progress".into(), Value::Null); }
            p.insert(OWN_PROGRESS.into(), Value::Null);
        }
        None => {}
    }
    // The refresher's own parts are replaced; everyone else's (the binding's question, the agent's buttons) stay.
    let mut own: Vec<Value> = Vec::new();
    if let Some(i) = &u.icon { p.insert("icon".into(), json!(i)); }
    if !u.parts.is_empty() {
        // With parts the app draws no progress bar of its own: it goes first.
        if let Some((d, t, _)) = progress { own.push(json!({"type": "progress", "value": d / t, OWN_PART: true})); }
        own.extend(u.parts.iter().cloned());
    }
    let others = o.get("parts").and_then(Value::as_array).into_iter().flatten().filter(|x| x.get(OWN_PART).is_none()).cloned();
    let parts: Vec<Value> = own.into_iter().chain(others).collect();
    p.insert("parts".into(), if parts.is_empty() { Value::Null } else { Value::Array(parts) });
    match &u.image {
        Some(i) => { p.insert("image".into(), json!(i)); }
        // A picture that went away would draw as a broken image.
        None => if o.get("image").and_then(Value::as_str).is_some_and(|i| !crate::islands::picture(i)) { p.insert("image".into(), Value::Null); },
    }
    if let Some(t) = &u.open { p.insert("open".into(), json!(t)); }
    for k in &u.clear { p.insert((*k).into(), Value::Null); }
    p.retain(|k, v| o.get(k).unwrap_or(&Value::Null) != v);
    p
}

/// What changed for the agent's notice.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Diff {
    pub added: Vec<String>,
    /// (title, where to: "Scheduled" or "Settled").
    pub moved: Vec<(String, String)>,
    pub ticked: Vec<String>,
    pub removed: Vec<String>,
    /// The ids minted for `added` (proposals since 2 Nov).
    pub added_ids: Vec<String>,
    /// The proposal this pass put on screen, for Heimdall's one short line.
    pub shown: Option<Map<String, Value>>,
}

/// Writes the plan as a three-way merge, holding the folder lock across both lists: `before` is what the model saw,
/// the files are what is there now. The model's changes (adds, moves, removals, text edits) apply only to items
/// unchanged since `before` (same list, same object); an item the user or the agent changed meanwhile (a tick, a
/// `plan_edit` move or edit) stays exactly as they left it, one they removed stays removed, and one they added stays.
/// `todo.json` and `goals.json` are never touched (Goals' since 31 Oct). Unknown keys on items and islands survive; a
/// missing list island is created; an old-shape island is migrated on the way. Then the rebuilt job islands, each in
/// one locked read-modify-write against the island as it is then (a removed island stays removed). Returns what the
/// refresher itself changed in the lists.
fn apply(root: &Root, _before: &Lists, plan: &Plan, jobs: &[Job]) -> Result<Diff> {
    // One system (`docs/one-system.md`): Scheduled holds only promises the user said yes to. What this pass
    // finds agreed with a real time is proposed (one shows, the rest queue); an item it leaves out, ticks or moves stays
    // exactly as he has it, and an item without a time is no promise and is dropped.
    let diff = root.locked(|| {
        let mut diff = Diff::default();
        let mut o = crate::goals::ops::current(root, LISTS[0].0).unwrap_or_else(|| lists::fresh(LISTS[0].0));
        let before = o.clone();
        let now = chrono::Utc::now().timestamp();
        for it in plan.lists[0].iter().filter(|it| it.id.is_empty() && !it.done) {
            let Some(due) = it.when.as_deref().and_then(|w| crate::islands::plan::due_seconds(w).ok()).filter(|d| *d > now) else { continue };
            let d = crate::islands::proposals::Draft { list: LISTS[0].0.into(), title: it.title.clone(), due: Some(due),
                why: it.detail.clone().unwrap_or_default(), by: "refresher".into(), ..Default::default() };
            match crate::islands::proposals::push(root, &mut o, &d, now) {
                Ok((id, shown)) => {
                    diff.added.push(it.title.clone());
                    diff.added_ids.push(id);
                    if shown { if let Some(p) = crate::islands::proposals::front(&o, now).0 { diff.shown = Some(p) } }
                }
                Err(e) => eprintln!("Plan refresher: '{}' not proposed: {e:#}", it.title),
            }
        }
        if o != before { lists::write_list(root, LISTS[0].0, &o)?; }
        Ok(diff)
    })?;
    for u in &plan.jobs {
        let Some(seen) = jobs.iter().find(|j| j.id == u.island) else { continue };
        root.locked(|| {
            let Some(mut o) = root.read(&u.island) else { return Ok(()) };
            // Three-way, per key: the model's change applies only to a key still as the model saw it; what the
            // agent, the binding or the user changed during the pass stays theirs.
            let mut patch = job_patch(&seen.island, u);
            let wrote_progress = patch.contains_key("progress");
            patch.retain(|k, _| o.get(k) == seen.island.get(k));
            // The mark goes with the value it marks: no mark on progress someone else changed meanwhile.
            if wrote_progress && !patch.contains_key("progress") { patch.remove(OWN_PROGRESS); }
            // A question that arrived during the pass keeps its state.
            if o.get("attention").and_then(Value::as_str) == Some("question") { patch.remove("state"); }
            // An unchanged island is not rewritten (the app redraws on every write).
            patch.retain(|k, v| o.get(k).unwrap_or(&Value::Null) != v);
            if patch.is_empty() { return Ok(()) }
            crate::islands::files::merge(&mut o, &patch);
            crate::islands::tools::stamp_now(&mut o, &patch);
            crate::islands::files::write_atomic(&root.file(&u.island), &Value::Object(o))
        })?;
    }
    Ok(diff)
}


/// The agent's notice, or `None` when nothing in the plan changed: "Plan: added "A"; moved "B" to Settled."
pub fn diff_text(d: &Diff) -> Option<String> {
    let q = |t: &String| format!("\u{201c}{t}\u{201d}");
    let mut parts: Vec<String> = Vec::new();
    if !d.added.is_empty() {
        parts.push(format!("proposed {} for Scheduled, waiting for User's yes{}", d.added.iter().map(q).collect::<Vec<_>>().join(", "),
            if d.shown.is_some() { " (one is on screen: say it in one short line; his yes is plan_edit accept, his no decline)" } else { "" }));
    }
    // Moves to one place read as one clause: moved "A", "B" to Settled.
    let mut places: Vec<&String> = Vec::new();
    for (_, to) in &d.moved { if !places.contains(&to) { places.push(to) } }
    for to in places {
        let titles: Vec<String> = d.moved.iter().filter(|(_, t)| t == to).map(|(title, _)| q(title)).collect();
        parts.push(format!("moved {} to {to}", titles.join(", ")));
    }
    if !d.ticked.is_empty() { parts.push(format!("ticked {}", d.ticked.iter().map(q).collect::<Vec<_>>().join(", "))); }
    if !d.removed.is_empty() { parts.push(format!("removed {}", d.removed.iter().map(q).collect::<Vec<_>>().join(", "))); }
    if parts.is_empty() { return None }
    let text = format!("Plan: {}.", parts.join("; "));
    Some(clip(&text, 700))
}

use crate::pi_session::clip;

#[cfg(test)]
#[path = "plan_refresher_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "plan_refresher_replay.rs"]
mod replay;
