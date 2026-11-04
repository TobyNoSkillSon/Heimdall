//! T3 wake policy (E7). Heimdall's agent is woken for two things only:
//! - a run that finished on a thread Heimdall started or sent a message into, with its result: those report through
//!   workers.rs (tasks and watched runs, with T3's waiting-for-children rule); `owned_threads` here is the
//!   pre-20-Oct record of threads launched through `t3_call`;
//! - a question or approval wait on any thread.
//! Finished runs of other threads wake nothing. Sub-agent and archived threads, the results of Heimdall's own
//! interrupts or archives, and everything found by the first pass after a start wake nothing.
//!
//! Each control-plane pass lists the active threads of every project plus the most recently finished ones and
//! compares them with the last pass (durable, so a restart catches what finished meanwhile). Event-backed waits
//! on active runs (workers.rs) trigger a pass the moment a run ends. Lines from one pass, and from passes less
//! than `WAKE_GAP_MS` apart, go out as one message. A finished line repeating the text last delivered for its
//! thread is dropped; every line id is `t3:{thread}:{run}:{kind}`, so a run reported twice wakes once.
use super::client::bounded_text;
use super::workers::Workers;
use agent::delivery::DeliveryPermit;
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;

const ACTIVE: [&str; 5] = ["preparing", "queued", "starting", "running", "waiting"];
const TERMINAL: [&str; 5] = ["completed", "failed", "interrupted", "cancelled", "rolled_back"];
/// At most one T3 wake per this interval; lines found meanwhile wait and go out together.
pub const WAKE_GAP_MS: u64 = 30_000;
/// Concurrent event-backed waits on other threads' runs.
pub const MAX_WAITS: usize = 16;
/// Lines in one wake; the rest are counted.
const WAKE_LINES: usize = 20;
// Bytes in one wake: `Actions::notice_budget`, from the running agent's message limit. Finished lines carry whole
// replies (up to `text::notice_reply_budget` of it each); lines past it wait for the next wake instead.
/// Characters per T3 read page while collecting a finished thread's reply.
const PAGE_CHARS: u64 = 4000;

pub(super) fn log_failure(e: &anyhow::Error) {
    let text = e.to_string();
    // Closed or unconnected T3 already has its own island state.
    if !text.starts_with("T3_UNAVAILABLE") && !text.starts_with("T3_AUTH_REQUIRED") && e.downcast_ref::<agent::delivery::DeliveryBlocked>().is_none() {
        eprintln!("T3 observer: {}", bounded_text(&format!("{e:#}"), 400));
    }
}
fn ms(text: &Value) -> u64 {
    text.as_str()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.timestamp_millis().max(0) as u64)
        .unwrap_or(0)
}
/// The titles of Heimdall's own relay threads (docs/claude-relay.md): the relay acting as his agent is never a thread
/// he follows, so its runs and approvals are never notices to him (bug note 2026-10-27T19-59-18: "the emergency cover
/// thread itself waits for an approval").
pub const RELAY_TITLES: &[&str] = &["Heimdall emergency cover", "Heimdall relay"];
/// The title prefix of a background job's one-call thread (`app::background_call::t3`): never followed either, so a
/// tool prompt the call stops at once is no notice to Heimdall.
pub const BACKGROUND_TITLE: &str = heimdall_emergency::approvals::BACKGROUND_PREFIX;
fn relay_thread(row: &Value) -> bool {
    row["title"].as_str().is_some_and(|t| RELAY_TITLES.iter().any(|r| t.starts_with(r)) || t.starts_with(BACKGROUND_TITLE))
}
/// One of Heimdall's relay threads by its title (not a background call's thread).
pub(crate) fn relay_title(title: &str) -> bool {
    RELAY_TITLES.iter().any(|r| title.starts_with(r))
}
/// A sub-agent thread T3's own `delegate_task` made (id `thread:delegated-task:…`, created over MCP), not one of a
/// provider's native sub-agents (Claude's Agent tool: `creationSource` "provider").
fn delegated_child(row: &Value) -> bool {
    row["relationshipToParent"] == "subagent"
        && (row["creationSource"] == "mcp" || row["threadId"].as_str().is_some_and(|t| t.starts_with("thread:delegated-task:")))
}
fn title(row: &Value) -> String {
    let t = row["title"].as_str().unwrap_or("").replace(['\'', '\n'], " ");
    bounded_text(if t.trim().is_empty() { "Untitled" } else { &t }, 60)
}
struct Seen {
    notified: Option<String>,
}
/// What Heimdall knows about T3 connection right now, for the app to show. Never delivered to the agent (E7.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    /// No connection attempt has completed yet in this process.
    Unknown,
    Connected,
    /// T3 Code is not running.
    Closed,
    /// No usable credential: never paired, expired, revoked, or rejected (401).
    SignInRequired,
}
/// One line for the next wake. `id` is `t3:{thread}:{run}:{kind}` (deterministic, so a repeat is one row).
struct Line {
    id: String,
    thread: String,
    /// A finished run (subject to the same-text rule); questions and approvals are always distinct requests.
    done: bool,
    text: String,
}

impl Workers {
    /// Tests: one observer pass, as the watcher's 30-second tick runs it.
    #[doc(hidden)]
    pub async fn observe_for_tests(&self, permit: &DeliveryPermit) -> Result<()> {
        self.observe(permit).await.map(|_| ())
    }
    fn meta(&self, key: &str) -> Result<Option<Value>> {
        let db = self.db.lock().unwrap();
        let v: Option<rusqlite::types::Value> =
            db.query_row("SELECT value FROM observer_meta WHERE key=?", [key], |r| r.get(0)).optional()?;
        Ok(v.map(|v| match v {
            rusqlite::types::Value::Integer(n) => json!(n),
            rusqlite::types::Value::Text(t) => json!(t),
            _ => Value::Null,
        }))
    }
    fn set_meta(&self, key: &str, value: impl rusqlite::ToSql) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT OR REPLACE INTO observer_meta VALUES(?,?)", params![key, value])?;
        Ok(())
    }
    /// The connection state the app shows.
    pub fn connection(&self) -> ConnectionState {
        *self.connection.lock().unwrap()
    }
    fn set_connection(&self, state: ConnectionState) {
        let mut current = self.connection.lock().unwrap();
        if *current != state {
            // heimdall.log keeps every change of T3's connection (night 20 Oct: log everything that goes wrong).
            eprintln!("t3: connection {:?} -> {state:?}", *current);
        }
        *current = state;
    }
    /// A 401 from a worker's read: the credential was revoked or rejected after the connection succeeded.
    pub(super) fn set_connection_sign_in(&self) {
        self.set_connection(ConnectionState::SignInRequired);
    }
    /// The island's "Connect T3 · click" card. Keyed by the credential, so it shows once per pairing state.
    pub(super) fn connect_prompt(&self, permit: &DeliveryPermit) -> Result<()> {
        self.actions.notice(&format!("t3-connect:{}", self.client.connection_notice_key()), "Connect T3 · click", "info", None, permit)?;
        Ok(())
    }
    /// The island shows when T3 Code is not reachable (once per outage) and when it is back. Returns whether
    /// T3 is offline. Also records the connection state.
    pub(super) fn reachability(&self, reach: &Result<()>, permit: &DeliveryPermit) -> Result<bool> {
        let offline = reach.as_ref().is_err_and(|e| e.to_string().starts_with("T3_UNAVAILABLE"));
        self.set_connection(match reach {
            Ok(()) => ConnectionState::Connected,
            Err(_) if offline => ConnectionState::Closed,
            Err(e) if e.to_string().starts_with("T3_AUTH_REQUIRED") => ConnectionState::SignInRequired,
            Err(_) => ConnectionState::Unknown,
        });
        let was = self.meta("offline")?.and_then(|v| v.as_str().map(str::to_owned));
        match (offline, was) {
            (true, None) => {
                let id = self.actions.notice(&format!("t3-offline:{}", uuid::Uuid::new_v4()), "T3 Code closed", "warning", None, permit)?;
                self.set_meta("offline", id)?;
            }
            (false, Some(id)) if reach.is_ok() => {
                self.actions.notice(&format!("{id}:back"), "T3 connected", "success", Some(&id), permit)?;
                self.db.lock().unwrap().execute("DELETE FROM observer_meta WHERE key='offline'", [])?;
            }
            _ => {}
        }
        Ok(offline)
    }
    fn seen(&self, thread: &str) -> Result<Option<Seen>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT notified FROM observed WHERE thread=?", [thread], |r| Ok(Seen { notified: r.get(0)? }))
            .optional()?)
    }
    fn record(&self, thread: &str, project: &str, title: &str, status: &str, run: Option<&str>, notified: Option<&str>) -> Result<()> {
        self.db.lock().unwrap().execute(
            "INSERT INTO observed(thread,project,title,status,run,notified,seen_ms) VALUES(?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(thread) DO UPDATE SET project=?2,title=?3,status=?4,run=?5,notified=COALESCE(?6,notified),seen_ms=?7",
            params![thread, project, title, status, run, notified, crate::now_ms() as i64],
        )?;
        Ok(())
    }
    /// `subagents`: the active listing includes sub-agent threads (few run at once; idle ones are not listed) for
    /// `island_runs`; they are left out of everything else.
    pub(super) async fn page_threads(&self, project: &str, statuses: &[&str], limit: u64, pages: usize, subagents: bool) -> Result<Vec<Value>> {
        let mut out = vec![];
        let mut cursor = Value::Null;
        for _ in 0..pages {
            // Sub-agent threads report to their parent thread; they would also crowd real threads out of a page.
            let mut args = json!({"projectId":project,"statuses":statuses,"limit":limit,"includeSubagents":subagents});
            if !cursor.is_null() {
                args["cursor"] = cursor.clone();
            }
            let page = self.client.call("t3_thread_list", args).await?;
            out.extend(page["threads"].as_array().context("T3 thread list missing")?.iter().cloned());
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        Ok(out)
    }
    /// One pass over every project. Returns active runs worth an event-backed wait, newest first.
    pub(super) async fn observe(&self, permit: &DeliveryPermit) -> Result<Vec<(String, String)>> {
        permit.check()?;
        let now = crate::now_ms();
        // The first successful pass of each process is a silent baseline (E7.4): nothing that ended before this
        // start is news, and lines queued before the restart are history (E4.4), not wakes.
        let startup = !self.started.load(Ordering::Acquire);
        let first = startup || self.meta("baseline")?.is_none();
        if first {
            self.set_meta("baseline", now as i64)?;
        }
        if startup {
            self.db.lock().unwrap().execute("DELETE FROM pending_wakes", [])?;
        }
        // Runs that ended after the previous pass (with margin) are news; older ones are history.
        let since = self.meta("last_pass")?.and_then(|v| v.as_u64()).map_or(now, |p| p.saturating_sub(120_000));
        let skip: HashSet<String> = self
            .db
            .lock()
            .unwrap()
            // Tasks and watched runs report through workers.rs; a watched thread is the observer's again afterwards.
            .prepare("SELECT thread FROM workers WHERE archived=0 OR kind='task'")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let ignored = |row: &Value| {
            row["threadId"].as_str().is_none_or(|t| skip.contains(t)) || row["relationshipToParent"] == "subagent" || row["archived"] == true || relay_thread(row)
        };
        let mut projects = vec![];
        let mut cursor = Value::Null;
        for _ in 0..10 {
            let mut args = json!({"limit":100});
            if !cursor.is_null() {
                args["cursor"] = cursor.clone();
            }
            let page = self.client.call("t3_project_list", args).await?;
            projects.extend(page["projects"].as_array().context("T3 projects missing")?.iter().filter_map(|p| p["id"].as_str().map(str::to_owned)));
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
        }
        let mut active: BTreeMap<String, (String, Value)> = BTreeMap::new();
        let mut recent = vec![];
        // Every row listed, before the filter: the runs that get a job island (`island_runs`).
        let mut listed = vec![];
        for project in &projects {
            permit.check()?;
            for row in self.page_threads(project, &ACTIVE, 100, 5, true).await? {
                if !ignored(&row) {
                    active.insert(row["threadId"].as_str().unwrap().to_owned(), (project.clone(), row.clone()));
                }
                listed.push(row);
            }
            for row in self.page_threads(project, &TERMINAL, 20, 1, false).await? {
                if !ignored(&row) {
                    recent.push((project.clone(), row.clone()));
                }
                listed.push(row);
            }
        }
        // Islands never fail a pass (its wakes matter more).
        if let Err(e) = self.island_runs(&listed).await {
            log_failure(&e);
        }
        if let Err(e) = self.record_own_threads(&listed) {
            log_failure(&e);
        }
        // Finished: active at the last pass and not now, or a recent terminal run not yet reported.
        let was_active: Vec<String> = {
            let db = self.db.lock().unwrap();
            let mut q = db.prepare(&format!(
                "SELECT thread FROM observed WHERE status IN ({})",
                ACTIVE.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",")
            ))?;
            let rows = q.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            rows
        };
        let mut finished: Vec<(String, bool)> = was_active.iter().filter(|t| !active.contains_key(*t) && !skip.contains(*t)).map(|t| (t.clone(), true)).collect();
        for (project, row) in &recent {
            let thread = row["threadId"].as_str().unwrap();
            let Some(run) = row["latestRunId"].as_str() else { continue };
            let seen = self.seen(thread)?;
            if seen.as_ref().and_then(|s| s.notified.as_deref()) == Some(&format!("done:{run}")) || finished.iter().any(|(t, _)| t == thread) {
                continue;
            }
            if first || ms(&row["updatedAt"]) <= since {
                // History, or the baseline pass: remember it as reported.
                self.record(thread, project, &title(row), row["status"].as_str().unwrap_or(""), Some(run), Some(&format!("done:{run}")))?;
                continue;
            }
            finished.push((thread.to_owned(), false));
        }
        let mut lines: Vec<Line> = vec![];
        for (thread, saw_running) in finished {
            permit.check()?;
            match self.finished_line(&thread, saw_running, !first, since).await {
                Ok(Some(line)) => lines.push(line),
                Ok(None) => {}
                // Deleted or unreadable: stop tracking it rather than failing every pass.
                Err(e) if e.to_string().starts_with("T3_TOOL_FAILED") => {
                    self.db.lock().unwrap().execute("UPDATE observed SET status='gone' WHERE thread=?", [&thread])?;
                }
                Err(e) => return Err(e),
            }
        }
        // Waiting threads: a question or an approval. One line per run and request set.
        for (thread, (project, row)) in &active {
            let run = row["latestRunId"].as_str();
            let status = row["status"].as_str().unwrap_or("running");
            let seen = self.seen(thread)?;
            let mut notified = None;
            // The same rule as the labels (`threads::asks`, bug note 2026-11-01T00-15-42): T3's "waiting", or a
            // question or approval counted while the run is live. Only active threads are here, so a finished
            // thread's leftover count never wakes.
            if crate::threads::asks(row) {
                permit.check()?;
                // One thread's failed read is retried next pass; it never costs the pass, whose finished lines are
                // already recorded as reported (review 2 Nov).
                let ids = match crate::question_ids(&self.client, thread).await {
                    Ok(ids) => ids,
                    Err(e) => {
                        log_failure(&e);
                        continue;
                    }
                };
                // A card Heimdall's permission hook already answered is no wait (`crate::waits_on_approval`).
                let waits = if ids.is_empty() {
                    match crate::waits_on_approval(&self.client, thread, row).await {
                        Ok(w) => w,
                        Err(e) => {
                            log_failure(&e);
                            continue;
                        }
                    }
                } else {
                    true
                };
                if !waits {
                    self.record(thread, project, &title(row), status, run, seen.as_ref().and_then(|s| s.notified.as_deref()))?;
                    continue;
                }
                let reqs = if ids.is_empty() { "approval".to_owned() } else { ids.join(",") };
                let key = format!("attention:{}:{reqs}", run.unwrap_or("idle"));
                if seen.as_ref().and_then(|s| s.notified.as_deref()) != Some(key.as_str()) {
                    if !first {
                        lines.push(Line {
                            id: format!("t3:{thread}:{}:attention:{reqs}", run.unwrap_or("idle")),
                            thread: thread.clone(),
                            done: false,
                            text: self.attention_line(thread, row, &ids).await?,
                        });
                    }
                    notified = Some(key);
                }
            }
            self.record(thread, project, &title(row), status, run, notified.as_deref())?;
        }
        if first {
            lines.clear();
        }
        self.deliver_lines(lines, now, permit)?;
        self.set_meta("last_pass", now as i64)?;
        self.started.store(true, Ordering::Release);
        // Forget long-finished threads, and the runs that got their island long ago.
        self.db.lock().unwrap().execute("DELETE FROM islanded WHERE ms<?", [(now - 7 * 24 * 60 * 60 * 1000) as i64])?;
        self.db.lock().unwrap().execute(
            &format!(
                "DELETE FROM observed WHERE status NOT IN ({}) AND seen_ms<?",
                ACTIVE.iter().map(|s| format!("'{s}'")).collect::<Vec<_>>().join(",")
            ),
            [(now - 7 * 24 * 60 * 60 * 1000) as i64],
        )?;
        let mut rows: Vec<&Value> = active.values().map(|(_, r)| r).collect();
        rows.sort_by_key(|r| std::cmp::Reverse(ms(&r["updatedAt"])));
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((r["threadId"].as_str()?.to_owned(), r["latestRunId"].as_str()?.to_owned())))
            .take(MAX_WAITS)
            .collect())
    }
    /// Bug note 2026-10-30T21-56-20 (the user: "any thread running for Heimdall gets its job island without a separate
    /// call, including when an old task thread starts a new run"): every active run of a thread working for Heimdall
    /// gets the task hook once (`run_started`), however it started. Those threads are Heimdall's own tasks (a run
    /// started by T3 itself or by a send from inside T3, which Heimdall's tools never see) and the children T3's own
    /// `delegate_task` made for the relay: the climbs of 30 Oct 21:11 were such children, started by the relay calling
    /// T3's `delegate_task` directly and later woken by sends. A thread Heimdall only sent a message into (the user's own)
    /// and a task's own sub-agents get none. The hook keeps an island still bound to the thread, so a live island is
    /// never doubled. Runs show here on the next pass: within 30 s, or at once when a followed run ends.
    pub(super) async fn island_runs(&self, rows: &[Value]) -> Result<()> {
        if !self.has_task_hook() {
            return Ok(());
        }
        {
            let db = self.db.lock().unwrap();
            for row in rows.iter().filter(|r| r["relationshipToParent"] != "subagent") {
                if let Some(thread) = row["threadId"].as_str() {
                    db.execute("INSERT OR REPLACE INTO relay_parents VALUES(?,?)", params![thread, relay_title(row["title"].as_str().unwrap_or(""))])?;
                }
            }
        }
        for row in rows.iter().filter(|r| ACTIVE.contains(&r["status"].as_str().unwrap_or("")) && r["archived"] != true) {
            let (Some(thread), Some(run)) = (row["threadId"].as_str(), row["latestRunId"].as_str()) else { continue };
            if self.islanded(thread, run)? {
                continue;
            }
            let title = if row["relationshipToParent"] != "subagent" && self.owns_task(thread)? {
                self.task_title(thread)?
            } else if self.relay_task(row).await? {
                crate::text::title(row["title"].as_str().unwrap_or("")).trim().to_owned()
            } else {
                continue;
            };
            self.run_started(thread, Some(run), if title.is_empty() { "Task" } else { &title }, "")?;
        }
        Ok(())
    }
    /// The approval hook answers `delegate_task` children of Heimdall's threads (bug note 2026-11-02T14-56-51-2) by the
    /// ids recorded for it. A thread launched since this build is recorded at launch (`Client::record_threads`); each
    /// pass also records the active threads that are Heimdall's tasks or its relay, so threads started earlier are
    /// covered while they can still delegate. Not every `owned_threads` row: a fork Heimdall made of one of the user's
    /// threads is his to work in, and the hook would decline his own prompts there (fresh review, 2 Nov).
    fn record_own_threads(&self, rows: &[Value]) -> Result<()> {
        let mut own = vec![];
        for row in rows.iter().filter(|r| ACTIVE.contains(&r["status"].as_str().unwrap_or(""))) {
            let Some(thread) = row["threadId"].as_str() else { continue };
            let relay = row["creationSource"] == "mcp" && relay_title(row["title"].as_str().unwrap_or(""));
            if relay || (row["relationshipToParent"] != "subagent" && self.owns_task(thread)?) {
                own.push(thread);
            }
        }
        self.client.record_threads(&own);
        Ok(())
    }
    /// Whether a thread (a T3 list row or read shell) is a T3 `delegate_task` child of one of Heimdall's relay threads.
    /// A parent not listed yet is read once and remembered.
    pub(super) async fn relay_task(&self, row: &Value) -> Result<bool> {
        let Some(parent) = row["parentThreadId"].as_str().filter(|_| delegated_child(row)) else { return Ok(false) };
        let known: Option<bool> = self.db.lock().unwrap().query_row("SELECT relay FROM relay_parents WHERE thread=?", [parent], |r| r.get(0)).optional()?;
        if let Some(relay) = known {
            return Ok(relay);
        }
        let relay = if self.owns_task(parent)? {
            false
        } else {
            let t = self.client.call("t3_thread_read", json!({"threadId":parent,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?;
            t["thread"]["parentThreadId"].is_null() && relay_title(t["thread"]["title"].as_str().unwrap_or(""))
        };
        self.db.lock().unwrap().execute("INSERT OR REPLACE INTO relay_parents VALUES(?,?)", params![parent, relay])?;
        Ok(relay)
    }
    /// The wake line for a finished run on a thread Heimdall started, or None when it wakes nothing: a run of any
    /// other thread, history (`news` false: the baseline pass), archived threads, and Heimdall's own stops.
    async fn finished_line(&self, thread: &str, saw_running: bool, news: bool, since: u64) -> Result<Option<Line>> {
        let mut args = json!({"threadId":thread,"view":"messages","runLimit":1,"limit":100,"maxCharsPerItem":PAGE_CHARS});
        let r = self.client.call("t3_thread_read", args.clone()).await?;
        let t = &r["thread"];
        let status = t["status"].as_str().unwrap_or("idle").to_owned();
        let project = t["projectId"].as_str().unwrap_or("").to_owned();
        let name = title(t);
        let run = t["latestRunId"].as_str().map(str::to_owned);
        if ACTIVE.contains(&status.as_str()) {
            // Still going (a new run started): the next wait or pass reports it.
            self.record(thread, &project, &name, &status, run.as_deref(), None)?;
            return Ok(None);
        }
        let key = format!("done:{}", run.as_deref().unwrap_or("none"));
        if self.seen(thread)?.and_then(|s| s.notified).as_deref() == Some(key.as_str()) {
            return Ok(None);
        }
        self.record(thread, &project, &name, &status, run.as_deref(), Some(&key))?;
        let completed = r["recentRuns"].as_array().into_iter().flatten().find(|x| x["runId"].as_str() == run.as_deref()).map(|x| ms(&x["completedAt"])).unwrap_or(0);
        let own_stop = matches!(status.as_str(), "interrupted" | "cancelled") && self.quiet(thread)?;
        let owned = self.owns(thread)?;
        let Some(run) = run else { return Ok(None) };
        if !owned || !news || t["archived"] == true || own_stop || (!saw_running && completed <= since) {
            return Ok(None);
        }
        // The run's last assistant message, from the last page.
        let mut page = r.clone();
        for _ in 0..5 {
            if page["hasMore"] != true {
                break;
            }
            args["afterPosition"] = page["nextPosition"].clone();
            page = self.client.call("t3_thread_read", args.clone()).await?;
        }
        let last = [&page, &r]
            .iter()
            .find_map(|p| p["items"].as_array().into_iter().flatten().rev().find(|i| i["type"] == "assistant_message").cloned());
        // The reply in full: Heimdall never needs a read just to see the answer. Pages stop once the
        // reply is past what a notice may carry: it is pointed to then.
        let budget = crate::text::notice_reply_budget(self.actions.notice_budget());
        let (said, whole) = match last {
            Some(item) => self.whole_text_in(thread, "messages", item, budget).await?,
            None => (String::new(), true),
        };
        let asks = if crate::threads::asks(t) { " It also waits on a question or approval." } else { "" };
        let said = said.trim();
        let text = if said.is_empty() {
            format!("- '{name}' (yours) {status} with no reply text.{asks} [thread {thread}]")
        } else if !whole || said.len() > budget {
            let size = if whole { format!("{} KB", said.len().div_ceil(1024)) } else { format!("over {} KB", budget / 1024) };
            format!("- '{name}' (yours) {status}.{asks} Its reply is {size}, too long for a notice; read it with t3_thread_read. [thread {thread}]")
        } else {
            format!("- '{name}' (yours) {status}.{asks} [thread {thread}] {}:\n{said}\n", if status == "completed" { "Final reply" } else { "Last reply" })
        };
        Ok(Some(Line { id: format!("t3:{thread}:{run}:done"), thread: thread.to_owned(), done: true, text }))
    }
    /// A message item's whole text, following T3's UTF-16 continuation (bounded like final collection), and whether
    /// it is whole: reading stops once the text is past `cap` bytes.
    pub(crate) async fn whole_text(&self, thread: &str, item: Value, cap: usize) -> Result<(String, bool)> {
        self.whole_text_in(thread, "messages", item, cap).await
    }
    /// `whole_text` for an item of `view` (an activity item is continued in the activity view).
    pub(crate) async fn whole_text_in(&self, thread: &str, view: &str, mut item: Value, cap: usize) -> Result<(String, bool)> {
        let mut text = item["text"].as_str().unwrap_or("").to_owned();
        while item["textTruncated"] == true {
            if text.len() > cap {
                return Ok((text, false));
            }
            let offset = item["nextTextOffset"].as_u64().context("T3 truncated item lacks UTF-16 continuation")?;
            anyhow::ensure!(text.len() <= 16 * 1024 * 1024, "T3 reply exceeds retention bound; read it in T3");
            let r = self.client.call("t3_thread_read", json!({"threadId":thread,"view":view,"itemId":item["itemId"],"textOffset":offset,"limit":1,"maxCharsPerItem":PAGE_CHARS,"runLimit":1})).await?;
            let next = r["items"].as_array().and_then(|a| a.first()).cloned().context("T3 continuation lost the reply")?;
            anyhow::ensure!(next["nextTextOffset"].as_u64().is_none_or(|n| n > offset), "T3 text cursor made no progress");
            text.push_str(next["text"].as_str().unwrap_or(""));
            item = next;
        }
        Ok((text, true))
    }
    async fn attention_line(&self, thread: &str, row: &Value, ids: &[String]) -> Result<String> {
        let name = title(row);
        let yours = if self.owns(thread)? { " (yours)" } else { "" };
        if let Some(id) = ids.first() {
            let q = crate::read_question(&self.client, thread, id).await?;
            return Ok(format!("- '{name}'{yours} asks [thread {thread}, request {id}]:\n{}", crate::text::question(&q)));
        }
        Ok(format!("- '{name}'{yours} waits for an approval in T3; only User can give it there [thread {thread}]"))
    }
    /// Queue lines and send one message when the last T3 wake is at least `WAKE_GAP_MS` old.
    fn deliver_lines(&self, lines: Vec<Line>, now: u64, permit: &DeliveryPermit) -> Result<()> {
        {
            let db = self.db.lock().unwrap();
            let tx = db.unchecked_transaction()?;
            for l in lines {
                // A finished run whose text repeats the last one queued for its thread is not news (E6.4).
                if l.done {
                    let digest = crate::body_digest(&l.text);
                    let last: Option<String> = tx.query_row("SELECT digest FROM last_result WHERE thread=?", [&l.thread], |r| r.get(0)).optional()?;
                    if last.as_deref() == Some(digest.as_str()) {
                        continue;
                    }
                    tx.execute("INSERT OR REPLACE INTO last_result VALUES(?,?)", params![l.thread, digest])?;
                }
                tx.execute("INSERT OR REPLACE INTO pending_wakes VALUES(?,?,?)", params![l.id, l.text, now as i64])?;
            }
            tx.commit()?;
        }
        let last = self.meta("last_wake")?.and_then(|v| v.as_u64()).unwrap_or(0);
        if now.saturating_sub(last) < WAKE_GAP_MS {
            return Ok(());
        }
        let pending: Vec<(String, String)> = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT key,line FROM pending_wakes ORDER BY created_ms,key")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if pending.is_empty() {
            return Ok(());
        }
        // Whole lines up to the byte budget (at least one); lines past it stay pending for the next wake.
        let wake_bytes = self.actions.notice_budget();
        let mut bytes = 0;
        let mut take = 0;
        for (_, l) in pending.iter().take(WAKE_LINES) {
            if take > 0 && bytes + l.len() > wake_bytes {
                break;
            }
            bytes += l.len();
            take += 1;
        }
        let deferred = take < pending.len().min(WAKE_LINES);
        let mut text = String::from("T3 update:\n");
        text.push_str(&pending.iter().take(take).map(|(_, l)| l.as_str()).collect::<Vec<_>>().join("\n"));
        if deferred {
            let rest = pending.len() - take;
            text.push_str(&if rest == 1 { "\n- 1 more update follows in the next message.".to_owned() } else { format!("\n- {rest} more updates follow in the next message.") });
        } else if pending.len() > WAKE_LINES {
            text.push_str(&format!("\n- and {} more; see t3_thread_list", pending.len() - WAKE_LINES));
        }
        let pending: Vec<(String, String)> = if deferred { pending.into_iter().take(take).collect() } else { pending };
        text.push('\n');
        text.push_str(crate::text::SILENCE_RULE);
        // Deterministic from the lines it carries, so a retry of the same batch is one outbox row. A batch that
        // carries a question or approval says so in its id: the agent harness classifies notices by id
        // (agent::notice::NoticeKind::classify), and a question waits for the user after a start where a finished run
        // only becomes context.
        let keys: Vec<&str> = pending.iter().map(|(k, _)| k.as_str()).collect();
        let asks = keys.iter().any(|k| k.contains(":attention:"));
        let id = format!("t3:batch:{}{}", if asks { "attention:" } else { "" }, crate::body_digest(&keys.join("\n")));
        // If the agent still rejects the batch as too long (its limit shrank since the lines were queued), each line's
        // first line goes instead, with where to read the rest.
        let short = format!(
            "T3 update:\n{}\nThe replies were too long for one message; read them with t3_thread_read.\n{}",
            pending.iter().map(|(_, l)| l.lines().next().unwrap_or("").trim_end_matches(" Final reply:").trim_end_matches(" Last reply:")).collect::<Vec<_>>().join("\n"),
            crate::text::SILENCE_RULE
        );
        self.actions.enqueue_with_short(&id, &text, Some(&short), permit)?;
        {
            let db = self.db.lock().unwrap();
            let tx = db.unchecked_transaction()?;
            for (key, _) in &pending {
                tx.execute("DELETE FROM pending_wakes WHERE key=?", [key])?;
            }
            tx.execute("INSERT OR REPLACE INTO observer_meta VALUES('last_wake',?)", [now as i64])?;
            tx.commit()?;
        }
        self.actions.drain(permit)
    }
}
#[cfg(test)]
mod relay_tests {
    #[test]
    fn heimdall_s_own_relay_threads_are_never_followed() {
        for t in super::RELAY_TITLES { assert!(super::relay_thread(&serde_json::json!({"title":t}))); }
        assert!(super::relay_thread(&serde_json::json!({"title":"Heimdall emergency cover (lab drill)"})));
        assert!(!super::relay_thread(&serde_json::json!({"title":"Heimdall: fix the menu"})));
        assert!(super::relay_thread(&serde_json::json!({"title":"[heimdall-bg] plan refresher"})), "a background call's thread");
    }
}
