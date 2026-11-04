//! Heimdall's tasks: T3 threads it started (`delegate_task`, or a launch or fork through `t3_call`) and runs of other
//! threads it sent a message into (`t3_thread_send`, a watched run). Each has an event-backed wait while it works and
//! a scan on every 30-second pass; when it is done, one notice brings its reply in full.
//!
//! "Done" is T3's own `task_status` rule (orchestration-v2 `delegatedTaskProgress`): a finished turn whose sub-agents
//! or background commands still run is `waiting_for_children`, not a result (bug note 18-32-31: a benchmark worker
//! woke Heimdall twice with "I'll pick this up when they report"). Outside clients cannot read T3's projection, so the
//! scan follows the same facts in the thread's activity: sub-agent items still open, and "Command running in
//! background" items without their "Command … finished" notification. Claude's sub-agents also finish while their
//! parent's turn runs, and T3 then queues a turn of its own to hand them over, starting with a notification (bug note
//! 19-19-06): a turn followed by such a turn, or one during which a background sub-agent closed, is progress, not the
//! result. Progress can carry results ("Two of three back: A…, B…"), so a turn withheld that way rides, reply and all,
//! in the thread's next notice, before the last turn's reply (review 21 Oct). A sub-agent resumed with SendMessage
//! re-opens in place and holds the task again. A failed tool call inside a running task is its own business and wakes
//! nothing (bug note 18-24-56). Heimdall's own stops wake nothing either.
//!
//! Nothing is held for good (review 20 Oct): a turn held for its sub-agents is delivered after `SUBAGENT_HOLD_MS`,
//! one held for background work after `BACKGROUND_HOLD_MS`, each naming what it still waits on. Once such a notice
//! has named every background command still running, later turns waiting only on them are progress again (bug note
//! 22-19-38), up to `SUBAGENT_HOLD_MS` after that notice. A sub-agent thread's question or approval is reported as
//! its task's (`subagent_requests`). A watched run that was queued is followed by its own run id: the turn it waited
//! behind ending is not its reply.
use super::client::Client;
use agent::{
    actions::Actions,
    delivery::{DeliveryGate, DeliveryPermit},
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
/// What a task's finished turn means for its sign-in episode (`Workers::auth_episode`).
enum AuthStep {
    None,
    Retry,
    Report(String),
}
/// What a finished run leaves for its notice.
#[derive(Default)]
pub(super) struct Final {
    title: Option<String>,
    reply: Option<String>,
    /// Withheld earlier turns whose replies `reply` now carries (or that had none): no longer withheld.
    carried: Vec<String>,
    /// This run's own last reply, without the earlier turns `reply` carries.
    own: Option<String>,
}
#[derive(Clone)]
pub struct Worker {
    pub thread: String,
    pub run: Option<String>,
    pub status: String,
}
pub struct Workers {
    pub client: Arc<Client>,
    pub(super) db: Mutex<Connection>,
    pub actions: Arc<Actions>,
    pub(super) changed: tokio::sync::Notify,
    /// Whether this process has completed an observer pass (observer.rs: the first pass is a silent baseline).
    pub(super) started: std::sync::atomic::AtomicBool,
    pub(super) connection: Mutex<super::observer::ConnectionState>,
    /// `CLEAR_GRACE_MS`, `BACKGROUND_HOLD_MS` and `SUBAGENT_HOLD_MS`; lab and test runs shorten them
    /// (`set_hold_timing`, `set_subagent_hold`).
    hold: Mutex<(u64, u64, u64)>,
    /// `TRAIL_MS`; tests shorten it (`set_trail`).
    trail: Mutex<u64>,
    /// The store's path: the restart notice is once per app process and store (`announce_restart`).
    path: std::path::PathBuf,
    /// One pass at a time: the watcher's passes and wait completions, and `heartbeat` from an app tick.
    pass: tokio::sync::Mutex<()>,
    /// The audit's outbox drain is failing: logged once until it succeeds again (`audit`).
    audit_drain_failing: std::sync::atomic::AtomicBool,
    /// Floating islands (night 1022): counts finished passes, so thread-bound islands re-read their threads after
    /// each one (`passes`; crates/app/src/islands/binding.rs).
    passes: tokio::sync::watch::Sender<u64>,
    /// Called with (thread, title, brief) for every task Heimdall starts (`delegate_task`, a launch or fork through
    /// `t3_call`), before the call returns: the app gives each one a bound job island (night 1023). What it returns
    /// is added to `delegate_task`'s answer.
    task_hook: Mutex<Option<TaskHook>>,
    /// The default system prompt for tasks (docs/skills.md, `agent::harness::WORKER_PROMPT`): put above every
    /// `delegate_task` brief, so the rules every brief carries never depend on Heimdall writing them.
    brief_preamble: Mutex<Option<String>>,
    /// `AUTH_RETRY_MS`; tests shorten it (`set_auth_retry`).
    auth_retry: Mutex<u64>,
    /// `AUTH_COOLDOWN_MS`; tests shorten it (`set_auth_cooldown`).
    auth_cooldown_ms: Mutex<u64>,
}
pub type TaskHook = Arc<dyn Fn(&str, &str, &str) -> Option<String> + Send + Sync>;
/// Stores whose restart notice this process has sent (`Workers::announce_restart`), with the notice's outbox id (none:
/// nothing was followed).
static ANNOUNCED: Mutex<Vec<(std::path::PathBuf, Option<String>)>> = Mutex::new(Vec::new());
/// Tests: the next `Workers` on the store at `path` behaves as in a new app process (its restart notice goes out
/// again). Other stores are untouched, so tests running in parallel threads do not reset each other.
#[doc(hidden)]
pub fn new_process_for_tests(path: &std::path::Path) {
    ANNOUNCED.lock().unwrap().retain(|(p, _)| p != path);
}
/// A finished turn whose only open work is background commands is reported after this long anyway, naming them:
/// a preview server left running for the user never ends, and its task must not stay silent for good. Short (bug note
/// 16-38-55, 21 Oct): a turn that ends "switch Developer Mode on; the Mac waits for the phone" leaves a background
/// waiter running until the user acts, so a long hold kept the very reply he had to act on for 35 minutes. A build
/// that ends within this long still comes as one notice.
pub const BACKGROUND_HOLD_MS: u64 = 3 * 60 * 1000;
/// How long a background command nothing can check (no output file known) still keeps a cleared task from being
/// archived: archiving stops the thread's Claude session and every process it started (bug note 2026-11-03T23-16-29).
pub const UNCHECKED_HOLD_MS: u64 = 6 * 60 * 60 * 1000;
/// After a watched run's result, the thread stays followed this long for the runs T3 starts by itself (a sub-agent
/// or background command of that work finishing), each reported; reset by each such notice. A run started by a
/// message (the user's own) ends the following at once (bug note 20-22-34: the 19:53 reply came three such runs after
/// the 19:34 one).
pub const TRAIL_MS: u64 = 60 * 60 * 1000;
/// Children all closed, the task still on the same finished turn this long: its result is delivered. T3 starts the
/// turn that reads a child's result within milliseconds of the child closing; this covers a scan that saw the child
/// close before T3 created that turn.
pub const CLEAR_GRACE_MS: u64 = 45_000;
/// A finished turn held back for its sub-agents is delivered after this long anyway, naming the sub-agents it still
/// waits on (review 20 Oct: a stuck sub-agent, or one waiting on a question nobody answers, must never hold a result
/// for good). Counted from when the turn was held; a new turn (T3 reading a child's result) starts it again.
pub const SUBAGENT_HOLD_MS: u64 = 30 * 60 * 1000;
/// Bug note 2026-10-24T23-34-21 (task mcp:14c20ff4): turns of a long-lived task failed at once on "Failed to
/// authenticate: OAuth session expired and could not be refreshed" from 23:34, while other threads worked, and a turn
/// started at 23:40 worked again. A task turn that fails on its provider's sign-in is retried once by itself this long
/// after the failure, instead of reporting the task failed.
pub const AUTH_RETRY_MS: u64 = 5 * 60 * 1000;
/// Bug note 2026-10-24T23-42-04: after such a failure, Heimdall's own sends to that thread wait this long (each would
/// stack behind the same failure; seven piled up on one worker).
pub const AUTH_COOLDOWN_MS: u64 = 10 * 60 * 1000;
/// One automatic retry per sign-in episode: a failure this long after the last one starts a new episode.
const AUTH_EPISODE_MS: u64 = 2 * 60 * 60 * 1000;
/// What the automatic retry says to the task.
pub const AUTH_RETRY_MESSAGE: &str = "Heimdall: your last turn failed on a sign-in error before it could work. Carry on with the task from where you left off.";
/// Activity items per page while following a task, and characters per item (enough to see the background marker
/// after a typical command line).
const ACTIVITY_PAGE: u64 = 50;
const ACTIVITY_CHARS: u64 = 1500;
/// How far into a cut command item its "running in background" line is looked for (`command_text`).
const COMMAND_CHARS: u64 = 16_000;
const BACKGROUND_MARK: &str = "Command running in background with ID: ";
/// A Claude Monitor stops by itself after its `timeout_ms`: 5 minutes unless it names one, at most 30 (Claude Code's
/// Monitor tool, 27 Oct). Its stop leaves a timeline item only when it starts a turn, so the deadline ends it too.
const MONITOR_TIMEOUT_MS: (i64, i64) = (5 * 60 * 1000, 30 * 60 * 1000);
/// How long a Monitor item without its task id yet holds the activity cursor before it (it normally has one within a
/// second).
const MONITOR_START_MS: u64 = 2 * 60 * 1000;
impl Workers {
    pub fn open(path: &Path, client: Arc<Client>, actions: Arc<Actions>) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
          CREATE TABLE IF NOT EXISTS retired_payloads(kind TEXT NOT NULL,id TEXT NOT NULL,reference TEXT NOT NULL,PRIMARY KEY(kind,id));
          CREATE TABLE IF NOT EXISTS pending_finals(thread TEXT NOT NULL,run TEXT NOT NULL,status TEXT NOT NULL,PRIMARY KEY(thread,run));
          CREATE TABLE IF NOT EXISTS active_activity(thread TEXT NOT NULL,item TEXT NOT NULL,PRIMARY KEY(thread,item));
          CREATE TABLE IF NOT EXISTS workers(thread TEXT PRIMARY KEY,run TEXT,status TEXT NOT NULL,provider TEXT NOT NULL,family TEXT NOT NULL,model TEXT NOT NULL,project TEXT NOT NULL,path TEXT NOT NULL,archived INTEGER NOT NULL DEFAULT 0,messages_cursor INTEGER,activity_cursor INTEGER);
          CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY,kind TEXT NOT NULL,args TEXT NOT NULL,state TEXT NOT NULL,result TEXT);
          CREATE TABLE IF NOT EXISTS notices(id TEXT PRIMARY KEY,thread TEXT NOT NULL,run TEXT,kind TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS questions(thread TEXT NOT NULL,id TEXT NOT NULL,PRIMARY KEY(thread,id));
          CREATE TABLE IF NOT EXISTS cursors(id TEXT PRIMARY KEY,thread TEXT NOT NULL,view TEXT NOT NULL,args TEXT NOT NULL,expires INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS final_items(thread TEXT NOT NULL,run TEXT NOT NULL,item TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(thread,run,item));")?;
        // The orchestrator pass-through (t3_call) and the all-threads observer (observer.rs).
        db.execute_batch("CREATE TABLE IF NOT EXISTS owned_threads(thread TEXT PRIMARY KEY);
          CREATE TABLE IF NOT EXISTS quiet(thread TEXT PRIMARY KEY,until_ms INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS consumed_confirms(id TEXT PRIMARY KEY);
          CREATE TABLE IF NOT EXISTS observed(thread TEXT PRIMARY KEY,project TEXT NOT NULL,title TEXT NOT NULL,status TEXT NOT NULL,run TEXT,notified TEXT,seen_ms INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS observer_meta(key TEXT PRIMARY KEY,value NOT NULL);
          CREATE TABLE IF NOT EXISTS pending_wakes(key TEXT PRIMARY KEY,line TEXT NOT NULL,created_ms INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS last_result(thread TEXT PRIMARY KEY,digest TEXT NOT NULL);")?;
        // Night 20 Oct: the sub-agents and background commands a task still waits on, and the finished turn held
        // back meanwhile (T3's waiting_for_children).
        db.execute_batch("CREATE TABLE IF NOT EXISTS children(thread TEXT NOT NULL,id TEXT NOT NULL,kind TEXT NOT NULL,label TEXT NOT NULL,since_ms INTEGER NOT NULL,PRIMARY KEY(thread,id));
          CREATE TABLE IF NOT EXISTS held(thread TEXT PRIMARY KEY,run TEXT NOT NULL,state TEXT NOT NULL,since_ms INTEGER NOT NULL,clear_ms INTEGER);")?;
        // Night 21 Oct (bug note 19-19-06): every sub-agent item a followed thread has had, open or closed (a closed one
        // can be re-opened in place, `recheck_subagents`), and the type of each run's first item (`started_by_message`).
        db.execute_batch("CREATE TABLE IF NOT EXISTS subagents(thread TEXT NOT NULL,item TEXT NOT NULL,label TEXT NOT NULL,position INTEGER,PRIMARY KEY(thread,item));
          CREATE TABLE IF NOT EXISTS run_starts(thread TEXT NOT NULL,run TEXT NOT NULL,kind TEXT NOT NULL,PRIMARY KEY(thread,run));")?;
        // Bug notes 2026-10-24T23-34-21 and 23-42-04: a task whose turn failed on its sign-in (`auth_failed`): the
        // failure's time, when its one automatic retry is due and when it went out; and Heimdall's own messages still
        // queued on a thread (`queued_sends`), which a later send joins instead of stacking another turn.
        db.execute_batch("CREATE TABLE IF NOT EXISTS auth_failures(thread TEXT PRIMARY KEY,run TEXT NOT NULL,failed_ms INTEGER NOT NULL,due_ms INTEGER NOT NULL,retried_ms INTEGER,reason TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS queued_sends(thread TEXT NOT NULL,run TEXT NOT NULL,text TEXT NOT NULL,PRIMARY KEY(thread,run));")?;
        // Review 21 Oct: turns that ended without a notice of their own because a later turn superseded them (T3's
        // hand-over turn, a held turn's successor, runs that ended between passes). Their replies ride in the next
        // notice of the thread, in order (`collect_final`), so no result is lost.
        db.execute_batch("CREATE TABLE IF NOT EXISTS withheld(thread TEXT NOT NULL,run TEXT NOT NULL,PRIMARY KEY(thread,run));")?;
        // The withheld turns a result notice carries whose delivery has not been confirmed: retired once it (or its
        // heartbeat resend) is delivered (`retire_carried`), so a resend still carries their replies (review 21 Oct).
        db.execute_batch("CREATE TABLE IF NOT EXISTS carried(notice TEXT NOT NULL,thread TEXT NOT NULL,run TEXT NOT NULL,PRIMARY KEY(notice,run));")?;
        // Bug note 01-28-46: a background command's output file (Claude's "Output is being written to: …"), whose
        // writer tells whether the command still runs (`settle_background`).
        if !db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('children') WHERE name='output')", [], |r| r.get::<_, bool>(0))? {
            db.execute_batch("ALTER TABLE children ADD COLUMN output TEXT")?;
        }
        // Bug note 22-19-38: when a delivered notice last named a background command as still running (`repeat`).
        if !db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('children') WHERE name='noticed_ms')", [], |r| r.get::<_, bool>(0))? {
            db.execute_batch("ALTER TABLE children ADD COLUMN noticed_ms INTEGER")?;
        }
        // Bug notes 2026-10-24T20-06-04 and 2026-10-25T21-22-04: a Claude Monitor is background work too; `until_ms`
        // is when it stops by itself (its `timeout_ms`), set only for monitors.
        if !db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('children') WHERE name='until_ms')", [], |r| r.get::<_, bool>(0))? {
            db.execute_batch("ALTER TABLE children ADD COLUMN until_ms INTEGER")?;
        }
        // When a thread's sub-agent or background command (not a monitor) was last seen closing: a turn T3 starts
        // after Heimdall last heard of the work may be handing that result over, which is news (`closes`). When a
        // notice last named each monitor description (`watch_notices`): the same watch re-armed is not news.
        db.execute_batch("CREATE TABLE IF NOT EXISTS closes(thread TEXT PRIMARY KEY,ms INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS watch_notices(thread TEXT NOT NULL,label TEXT NOT NULL,noticed_ms INTEGER NOT NULL,PRIMARY KEY(thread,label));")?;
        // Bug note 2026-10-30T21-56-20 ("any thread running for Heimdall gets its job island without a separate
        // call"): the runs the task hook already ran for (`run_started`), so a pass never makes a second island for
        // a run, and whether a thread is one of Heimdall's relay threads (`relay_parents`, read once per parent).
        db.execute_batch("CREATE TABLE IF NOT EXISTS islanded(thread TEXT NOT NULL,run TEXT NOT NULL,ms INTEGER NOT NULL,PRIMARY KEY(thread,run));
          CREATE TABLE IF NOT EXISTS relay_parents(thread TEXT PRIMARY KEY,relay INTEGER NOT NULL);")?;
        // ("figure out why they are not closing themself after the islands … have been cleared"):
        // tasks whose job island the user closed, archived by Heimdall itself once finished and reported
        // (`archive_cleared`), never by the agent remembering to.
        db.execute_batch("CREATE TABLE IF NOT EXISTS cleared(thread TEXT PRIMARY KEY,ms INTEGER NOT NULL);")?;
        let column = |name: &str| -> Result<bool> {
            Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('workers') WHERE name=?)", [name], |r| r.get(0))?)
        };
        // tool-output: the task's title tells tasks apart in task_status and in its finished notice.
        if !column("title")? {
            db.execute_batch("ALTER TABLE workers ADD COLUMN title TEXT")?;
        }
        // 'task': Heimdall started the thread. 'watch': someone else's thread, followed for the run Heimdall sent a
        // message into ("if I have to wake you up and then tell you to read it … that's just stupid").
        if !column("kind")? {
            db.execute_batch("ALTER TABLE workers ADD COLUMN kind TEXT NOT NULL DEFAULT 'task'")?;
        }
        // A watched thread: the run Heimdall's message went into, until that run has ended (a queued send waits
        // behind the thread's current turn, whose ending is not the reply).
        if !column("target_run")? {
            db.execute_batch("ALTER TABLE workers ADD COLUMN target_run TEXT")?;
        }
        // Night 22 Oct (bug note 20-22-34): after a watched run's result, the thread's own follow-up runs (started by
        // T3's notifications: sub-agents or background commands finishing) are still the answer to Heimdall's message.
        // `trail_run`: the last run whose notice went out; `trail_until`: following ends this long after it (ms).
        if !column("trail_until")? {
            db.execute_batch("ALTER TABLE workers ADD COLUMN trail_until INTEGER; ALTER TABLE workers ADD COLUMN trail_run TEXT")?;
        }
        Ok(Self {
            client,
            db: Mutex::new(db),
            actions,
            changed: tokio::sync::Notify::new(),
            started: std::sync::atomic::AtomicBool::new(false),
            connection: Mutex::new(super::observer::ConnectionState::Unknown),
            hold: Mutex::new((CLEAR_GRACE_MS, BACKGROUND_HOLD_MS, SUBAGENT_HOLD_MS)),
            trail: Mutex::new(TRAIL_MS),
            path: path.to_owned(),
            pass: tokio::sync::Mutex::new(()),
            audit_drain_failing: std::sync::atomic::AtomicBool::new(false),
            passes: tokio::sync::watch::channel(0).0,
            task_hook: Mutex::new(None),
            brief_preamble: Mutex::new(None),
            auth_retry: Mutex::new(AUTH_RETRY_MS),
            auth_cooldown_ms: Mutex::new(AUTH_COOLDOWN_MS),
        })
    }
    /// Sets the text put above every `delegate_task` brief (`brief_preamble`).
    pub fn set_brief_preamble(&self, text: &str) {
        *self.brief_preamble.lock().unwrap() = Some(text.trim().to_owned()).filter(|t| !t.is_empty());
    }
    /// Floating islands (night 1022): changes after every control-plane pass and every followed run's wait, the
    /// moments T3 state is fresh.
    pub fn passes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.passes.subscribe()
    }
    /// Run `hook` for every task started from now on (see `task_hook`); a later call replaces it.
    pub fn on_task_started(&self, hook: TaskHook) { *self.task_hook.lock().unwrap() = Some(hook) }
    /// The hook for a started task; its note after a space, or nothing.
    pub(super) fn task_started(&self, thread: &str, title: &str, brief: &str) -> String {
        let hook = self.task_hook.lock().unwrap().clone();
        hook.and_then(|h| h(thread, title, brief)).map(|n| format!(" {n}")).unwrap_or_default()
    }
    /// `task_started` for one run of the thread, remembered (`islanded`) so the observer's pass does not run the hook
    /// again for it: a run whose island the user already dismissed stays without one.
    pub(super) fn run_started(&self, thread: &str, run: Option<&str>, title: &str, brief: &str) -> Result<String> {
        if let Some(run) = run {
            self.db.lock().unwrap().execute("INSERT OR IGNORE INTO islanded VALUES(?,?,?)", params![thread, run, crate::now_ms() as i64])?;
        }
        Ok(self.task_started(thread, title, brief))
    }
    /// Whether the task hook already ran for this run (`run_started`).
    pub(super) fn islanded(&self, thread: &str, run: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM islanded WHERE thread=? AND run=?)", params![thread, run], |r| r.get(0))?)
    }
    pub(super) fn has_task_hook(&self) -> bool {
        self.task_hook.lock().unwrap().is_some()
    }
    /// The title Heimdall started a task with (T3's own carries the `[heimdall:…]` marker).
    pub(super) fn task_title(&self, thread: &str) -> Result<String> {
        Ok(self.title(thread, None)?.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| "Task".into()))
    }
    fn pass_done(&self) {
        self.passes.send_modify(|n| *n = n.wrapping_add(1));
    }
    /// Shorter hold timings for lab and test runs: (children-closed grace, background-only hold), in ms.
    pub fn set_hold_timing(&self, clear_grace_ms: u64, background_hold_ms: u64) {
        let mut hold = self.hold.lock().unwrap();
        (hold.0, hold.1) = (clear_grace_ms, background_hold_ms);
    }
    /// When the earliest held result's children-closed grace ends (wall-clock ms): the watcher's pass at that moment
    /// delivers it, instead of the next 30-second pass (bug note 22 Oct 15:35).
    /// The earliest end of a held result's grace after `fired` (the last one that already started a pass): a held row
    /// that outlives its own end never keeps the later ones waiting for the 30-second tick.
    fn hold_due(&self, fired: Option<u64>) -> Option<u64> {
        let grace = self.hold.lock().unwrap().0;
        let after = fired.map_or(-1, |f| f.saturating_sub(grace).min(i64::MAX as u64) as i64);
        let clear: Option<i64> = self.db.lock().unwrap().query_row("SELECT MIN(clear_ms) FROM held WHERE clear_ms IS NOT NULL AND clear_ms>?1", [after], |r| r.get(0)).ok().flatten();
        clear.map(|c| (c.max(0) as u64).saturating_add(grace))
    }
    /// A shorter `TRAIL_MS` for lab and test runs.
    pub fn set_trail(&self, ms: u64) {
        *self.trail.lock().unwrap() = ms;
    }
    /// A shorter `AUTH_RETRY_MS` for lab and test runs.
    pub fn set_auth_retry(&self, ms: u64) {
        *self.auth_retry.lock().unwrap() = ms;
    }
    /// A shorter `AUTH_COOLDOWN_MS` for lab and test runs.
    pub fn set_auth_cooldown(&self, ms: u64) {
        *self.auth_cooldown_ms.lock().unwrap() = ms;
    }
    /// A shorter `SUBAGENT_HOLD_MS` for lab and test runs.
    pub fn set_subagent_hold(&self, ms: u64) {
        self.hold.lock().unwrap().2 = ms;
    }
    pub fn retire(&self)->Result<()> {
        let db=self.db.lock().unwrap();
        let rows=db.prepare("SELECT id,args,result FROM operations WHERE state='complete' AND NOT EXISTS(SELECT 1 FROM retired_payloads WHERE kind='operation' AND retired_payloads.id=operations.id) LIMIT 100")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id,args,result) in rows {if let Some(reference)=self.actions.archive_payload(&json!({"store":"worker_operation","id":id,"args":args,"result":result}))?{let tx=db.unchecked_transaction()?;tx.execute("INSERT OR REPLACE INTO retired_payloads VALUES('operation',?,?)",params![id,reference])?;tx.execute("UPDATE operations SET args=? WHERE id=?",params![crate::body_digest(&args),id])?;tx.commit()?;}}
        let rows=db.prepare("SELECT thread,run,item,body FROM final_items WHERE EXISTS(SELECT 1 FROM notices WHERE kind='terminal' AND notices.thread=final_items.thread AND notices.run=final_items.run) AND NOT EXISTS(SELECT 1 FROM pending_finals WHERE pending_finals.thread=final_items.thread AND pending_finals.run=final_items.run) AND body NOT LIKE 'archive:%' LIMIT 100")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (thread,run,item,body) in rows {if let Some(reference)=self.actions.archive_payload(&json!({"store":"worker_final","thread":thread,"run":run,"item":item,"body":body}))?{db.execute("UPDATE final_items SET body=? WHERE thread=? AND run=? AND item=?",params![format!("archive:{reference}"),thread,run,item])?;}}
        Ok(())
    }
    /// One of Heimdall's own tasks (not a watched thread).
    fn task(&self, id: &str) -> Result<Worker> {
        self.db
            .lock()
            .unwrap()
            .query_row("SELECT thread,run,status FROM workers WHERE thread=? AND kind='task'", [id], |r| {
                Ok(Worker { thread: r.get(0)?, run: r.get(1)?, status: r.get(2)? })
            })
            .optional()?
            .with_context(|| format!("TASK_NOT_OWNED: {id} is not one of your tasks; read or message other threads with t3_thread_read and t3_thread_send"))
    }
    /// Whether Heimdall started this thread (a task), for "your task" marks.
    pub(crate) fn owns_task(&self, thread: &str) -> Result<bool> {
        Ok(self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM workers WHERE thread=? AND kind='task')", [thread], |r| r.get(0))?)
    }
    /// The task's live T3 title, or the one it was started with.
    fn title(&self, thread: &str, live: Option<&str>) -> Result<Option<String>> {
        if let Some(t) = live.filter(|t| !t.trim().is_empty()) {
            return Ok(Some(t.to_owned()));
        }
        Ok(self.db.lock().unwrap().query_row("SELECT title FROM workers WHERE thread=?", [thread], |r| r.get::<_, Option<String>>(0)).optional()?.flatten())
    }
    fn kind(&self, thread: &str) -> Result<String> {
        Ok(self.db.lock().unwrap().query_row("SELECT kind FROM workers WHERE thread=?", [thread], |r| r.get(0)).optional()?.unwrap_or_else(|| "task".into()))
    }
    /// Followed runs that are working: each gets an event-backed wait.
    pub fn active(&self) -> Result<Vec<Worker>> {
        Ok(self.db.lock().unwrap().prepare("SELECT thread,run,status FROM workers WHERE archived=0 AND status IN ('preparing','queued','starting','running','waiting')")?.query_map([],|r|Ok(Worker{thread:r.get(0)?,run:r.get(1)?,status:r.get(2)?}))?.collect::<rusqlite::Result<_>>()?)
    }
    fn all(&self) -> Result<Vec<Worker>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT thread,run,status FROM workers WHERE archived=0")?
            .query_map([], |r| Ok(Worker { thread: r.get(0)?, run: r.get(1)?, status: r.get(2)? }))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub(super) fn prepare(
        &self,
        id: &str,
        kind: &str,
        args: &Value,
        permit: &DeliveryPermit,
    ) -> Result<Option<Value>> {
        permit.check()?;
        let db = self.db.lock().unwrap();
        let body = args.to_string();
        if let Some((k, a, state, result)) = db
            .query_row("SELECT kind,args,state,result FROM operations WHERE id=?", [id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, Option<String>>(3)?))
            })
            .optional()?
        {
            ensure!(
                k == kind && (a == body || a == crate::body_digest(&body)),
                "REPLAY_CONFLICT: operation identity reused for other arguments"
            );
            if state == "complete" {
                return Ok(Some(serde_json::from_str(&result.unwrap())?));
            }
            if launch_kind(kind) && state.starts_with("launching:") {
                anyhow::bail!("LAUNCH_UNCERTAIN: a durable launch intent exists. Reconcile its title marker in T3 before a new launch; never retry blindly");
            }
        } else {
            permit.with(|| {
                db.execute("INSERT INTO operations VALUES(?1,?2,?3,'prepared',NULL)", params![id, kind, body])?;
                Ok(())
            })?;
        }
        Ok(None)
    }
    pub(super) fn finish(&self, id: &str, result: &Value) -> Result<()> {
        self.db.lock().unwrap().execute(
            "UPDATE operations SET state='complete',result=? WHERE id=?",
            params![result.to_string(), id],
        )?;
        Ok(())
    }
    /// The model selection for `target` (`delegate_task`): the model id, its provider (found from the model when not
    /// given) and options checked against the live catalog. A wrong id answers with the usable models, so the next
    /// call can be right without a separate catalog read.
    async fn selection(&self, target: &Value) -> Result<(Value, String, String)> {
        let catalog = self.client.call("orchestrator_capabilities", json!({})).await?;
        let providers = catalog["providers"].as_array().context("T3 capabilities lack providers")?;
        let model_id = target["model"].as_str().context("target.model missing")?;
        let choices = || crate::text::catalog(&catalog);
        let usable = |p: &&Value| {
            !p["constraints"].as_array().into_iter().flatten().any(|c| c.as_str().is_some_and(|c| c.contains("disabled")))
        };
        let candidates: Vec<&Value> = providers
            .iter()
            .filter(usable)
            .filter(|p| target["providerInstanceId"].as_str().is_none_or(|id| p["providerInstanceId"] == id))
            .filter(|p| p["models"].as_array().is_some_and(|m| m.iter().any(|m| m["id"] == model_id)))
            .collect();
        let provider = match candidates.as_slice() {
            [one] => *one,
            [] => anyhow::bail!("MODEL_UNAVAILABLE: {model_id} is not in T3's live catalog. Usable models:\n{}", choices()),
            _ => anyhow::bail!(
                "MODEL_AMBIGUOUS: {model_id} is offered by {}; set target.providerInstanceId",
                candidates.iter().filter_map(|p| p["providerInstanceId"].as_str()).collect::<Vec<_>>().join(" and ")
            ),
        };
        let provider_id = provider["providerInstanceId"].as_str().context("T3 provider id missing")?.to_owned();
        ensure!(
            provider["runtimeModes"].is_null() || provider["runtimeModes"].as_array().is_some_and(|m| m.iter().any(|x| x == "full-access")),
            "PERMISSION_UNAVAILABLE: {provider_id} does not support full-access"
        );
        let model = provider["models"].as_array().unwrap().iter().find(|m| m["id"] == model_id).unwrap();
        let mut options = serde_json::Map::new();
        for (id, value) in target["options"].as_object().into_iter().flatten() {
            let supported = model["options"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["id"] == id.as_str())
                .with_context(|| format!("OPTION_UNAVAILABLE: {model_id} has no option {id}. Its options:\n{}", crate::text::catalog_line(&json!({"provider":provider_id,"model":model}))))?;
            let choices = crate::text::option_choices(supported);
            if !choices.is_empty() && choices.iter().all(|(v, _)| v.is_boolean()) {
                ensure!(value.is_boolean(), "OPTION_INVALID: {id} takes true or false");
            } else {
                ensure!(
                    choices.iter().any(|(v, _)| v == value),
                    "OPTION_INVALID: {id} takes one of {}",
                    choices.iter().map(|(v, _)| crate::text::choice_text(v)).collect::<Vec<_>>().join("|")
                );
            }
            options.insert(id.clone(), value.clone());
        }
        // Driver identity identifies a family; unknown drivers only switch within their own instance.
        let driver = provider["driverKind"].as_str().unwrap_or("").to_ascii_lowercase();
        let family = if driver.contains("claude") {
            "claude".into()
        } else if driver.contains("codex") {
            "codex".into()
        } else {
            format!("instance:{provider_id}")
        };
        let mut selection = json!({"instanceId":provider_id,"model":model_id});
        if !options.is_empty() {
            selection["options"] = Value::Object(options);
        }
        Ok((selection, family, provider_id))
    }
    async fn project(&self, path: &str) -> Result<Value> {
        let path = std::fs::canonicalize(path).context("PROJECT_NOT_FOUND: path must exist")?;
        let mut cursor = None;
        let mut best: Option<Value> = None;
        for _ in 0..100 {
            let mut args = json!({"limit":100});
            if let Some(c) = cursor {
                args["cursor"] = c;
            }
            let page = self.client.call("t3_project_list", args).await?;
            for p in page["projects"].as_array().context("T3 projects missing")? {
                let Some(root) = p["workspaceRoot"].as_str() else {
                    continue;
                };
                let Ok(root_path) = std::fs::canonicalize(root) else {
                    continue;
                };
                if path.starts_with(&root_path)
                    && best.as_ref().is_none_or(|b| b["workspaceRoot"].as_str().unwrap_or("").len() < root.len())
                {
                    best = Some(p.clone());
                }
            }
            if page["nextCursor"].is_null() {
                break;
            }
            cursor = Some(page["nextCursor"].clone());
        }
        best.with_context(|| format!("PROJECT_NOT_FOUND: no registered T3 project contains {}; use the Vault or another registered project folder", path.display()))
    }
    pub async fn execute(&self, name: &str, a: Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        permit.check()?;
        match name {
            "delegate_task" => self.delegate(a, op, permit).await,
            "task_status" => self.task_status(&a).await,
            "task_cancel" => self.task_cancel(&a, op, permit).await,
            "t3_thread_list" => self.thread_list(&a).await,
            "t3_thread_read" => self.thread_read(&a).await,
            "t3_thread_send" => {
                if let Some(thread) = a["threadId"].as_str() {
                    if let Some(wait) = self.auth_cooldown(thread)? {
                        anyhow::bail!(wait);
                    }
                    // A replay of this operation: a join answers as it did; a send goes the usual way (`t3_call`).
                    let done: Option<(String, Option<String>)> = self.db.lock().unwrap().query_row("SELECT kind,result FROM operations WHERE id=?", [op], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                    if let Some(("t3_thread_join", Some(result))) = done.as_ref().map(|(k, r)| (k.as_str(), r.clone())) {
                        return Ok(serde_json::from_str::<Value>(&result)?["text"].as_str().unwrap_or_default().to_owned());
                    }
                    if done.is_none() && !matches!(a["mode"].as_str(), Some("steer" | "restart")) {
                        if let Some(joined) = self.join_queued(thread, a["message"].as_str().unwrap_or(""), permit).await? {
                            self.db.lock().unwrap().execute(
                                "INSERT OR REPLACE INTO operations VALUES(?1,'t3_thread_join',?2,'complete',?3)",
                                params![op, a.to_string(), json!({"text": joined}).to_string()],
                            )?;
                            return Ok(joined);
                        }
                    }
                }
                let mut call = json!({"tool":"t3_thread_send","args":{"threadId":a["threadId"],"message":a["message"]}});
                if let Some(mode) = a.get("mode") {
                    call["args"]["mode"] = mode.clone();
                }
                if let Some(confirm) = a.get("confirm") {
                    call["confirm"] = confirm.clone();
                }
                self.t3_call(call, op, permit).await
            }
            "t3_tools" => self.t3_tools(&a).await,
            "t3_call" => {
                if let (Some("t3_thread_send"), Some(thread)) = (a["tool"].as_str(), a["args"]["threadId"].as_str()) {
                    if let Some(wait) = self.auth_cooldown(thread)? {
                        anyhow::bail!(wait);
                    }
                }
                self.t3_call(a, op, permit).await
            }
            other => anyhow::bail!("unknown T3 tool {other}"),
        }
    }
    /// `delegate_task`: a full-access thread in the deepest registered project containing `path`, with a durable
    /// launch intent so a lost response never launches twice.
    async fn delegate(&self, a: Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        self.reconcile_launches(permit).await?;
        if let Some(result) = self.prepare(op, "delegate_task", &a, permit)? {
            // A replay: the hooks are idempotent (an island already bound to the thread is kept).
            let note = self.task_started(result["task_id"].as_str().unwrap_or(""), result["title"].as_str().unwrap_or(""), a["task"].as_str().unwrap_or(""));
            return Ok(started_line(&result) + &note);
        }
        let (selection, family, provider) = self.selection(&a["target"]).await?;
        let path = match a["path"].as_str() {
            Some(p) => p.to_owned(),
            None => default_path().context("path: no default project folder ($HOME/Work); pass path")?,
        };
        let project = self.project(&path).await?;
        let marker = format!("[heimdall:{}]", uuid::Uuid::new_v4().simple());
        let path = std::fs::canonicalize(&path)?;
        let title = a["title"].as_str().unwrap_or("Task").to_owned();
        let (workspace, task, made) = workspace(&project, &path, a["isolation"] == "worktree", a.get("branch"), a["task"].as_str().unwrap_or(""), &title, op).await?;
        // The default task prompt on top; the working-folder line, when there is one, belongs to the task below it.
        let task = with_preamble(self.brief_preamble.lock().unwrap().as_deref(), &task);
        // The task's working folder: the worktree Heimdall made for it, or the folder it was given.
        let path = made.as_ref().map_or(path, |(folder, _)| folder.clone());
        let model = selection["model"].as_str().unwrap_or("").to_owned();
        let args = json!({"projectId":project["id"],"title":format!("{title} {marker}"),"message":task,"modelSelection":selection,"runtimeMode":"full-access","workspaceStrategy":workspace});
        self.db.lock().unwrap().execute(
            "UPDATE operations SET state=?,result=? WHERE id=?",
            params![
                format!("launching:{marker}"),
                json!({"project":project["id"],"family":family,"path":path,"provider":provider,"model":model,"title":title}).to_string(),
                op
            ],
        )?;
        permit.check()?;
        let r = self.client.call("t3_thread_launch", args).await?;
        let thread = r["threadId"].as_str().context("T3 launch lacks thread ID")?;
        let status = r["status"].as_str().unwrap_or("preparing");
        self.db.lock().unwrap().execute(
            "INSERT OR REPLACE INTO workers(thread,run,status,provider,family,model,project,path,title,kind,archived) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'task',0)",
            params![thread, r["runId"].as_str(), status, provider, family, model, project["id"].as_str(), path.to_string_lossy(), title],
        )?;
        let mut result = json!({"task_id":thread,"status":status,"title":title,"model":model,"options":selection["options"]});
        if let Some((folder, branch)) = &made {
            result["worktree"] = json!(folder);
            result["branch"] = json!(branch);
        }
        self.finish(op, &result)?;
        self.changed.notify_one();
        let note = self.run_started(thread, r["runId"].as_str(), &title, a["task"].as_str().unwrap_or(""))?;
        permit.check()?;
        Ok(started_line(&result) + &note)
    }
    /// Recover a lost launch response without reissuing the non-idempotent launch.
    pub async fn reconcile_launches(&self, permit: &DeliveryPermit) -> Result<()> {
        let intents=self.db.lock().unwrap().prepare("SELECT id,args,state,result FROM operations WHERE kind IN ('delegate_task','t3_worker_start') AND state LIKE 'launching:%'")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (op, args, state, intent) in intents {
            permit.check()?;
            let a: Value = serde_json::from_str(&args)?;
            let intent: Value = serde_json::from_str(&intent)?;
            // Intents written before 20 Oct (t3_worker_start) carry provider, model and title in their arguments.
            let field = |k: &str| intent[k].as_str().or(a[k].as_str()).unwrap_or("").to_owned();
            let (provider, model, title) = (field("provider"), field("model"), field("title"));
            let marker = state.strip_prefix("launching:").unwrap();
            let page = self.client.call("t3_thread_list", json!({"projectId":intent["project"],"titleContains":marker,"limit":100})).await?;
            let candidates = page["threads"]
                .as_array()
                .context("T3 launch recovery lacks thread list")?
                .iter()
                .filter(|t| {
                    t["title"].as_str().is_some_and(|s| s.contains(marker))
                        && t["providerInstanceId"] == provider.as_str()
                        && t["model"] == model.as_str()
                        && t["runtimeMode"] == "full-access"
                })
                .collect::<Vec<_>>();
            ensure!(candidates.len()<=1,"LAUNCH_AMBIGUOUS: multiple threads match the durable launch intent; inspect T3 before starting more work");
            if let Some(t) = candidates.first() {
                let id = t["threadId"].as_str().context("T3 launch recovery thread ID missing")?;
                let status = t["status"].as_str().context("T3 launch recovery status missing")?;
                self.db.lock().unwrap().execute("INSERT OR IGNORE INTO workers(thread,run,status,provider,family,model,project,path,title,kind) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,'task')",params![id,t["latestRunId"].as_str(),status,provider,intent["family"].as_str(),model,intent["project"].as_str(),intent["path"].as_str(),title])?;
                self.finish(&op,&json!({"task_id":id,"status":status,"title":title,"model":model,"recovered":true}))?;
            }
        }
        Ok(())
    }
    /// The sub-agents and background commands a followed thread still waits on. Background commands older than
    /// `BACKGROUND_HOLD_MS` no longer hold a result back.
    fn open_children(&self, thread: &str) -> Result<Open> {
        let now = crate::now_ms();
        // A monitor past its own timeout has stopped, whether or not T3 showed the stop.
        self.db.lock().unwrap().execute("DELETE FROM children WHERE thread=? AND until_ms IS NOT NULL AND until_ms<=?", params![thread, now as i64])?;
        let rows: Vec<(String, String, i64, bool, Option<i64>, bool)> = self
            .db
            .lock()
            .unwrap()
            // A monitor counts as checked: its own timeout bounds it. (Its output file proves nothing: Claude writes a
            // monitor's events there itself, so no process holds it open while the monitor runs; checked 27 Oct.)
            .prepare("SELECT kind,label,since_ms,output IS NOT NULL OR until_ms IS NOT NULL,noticed_ms,until_ms IS NOT NULL FROM children WHERE thread=? ORDER BY since_ms")?
            .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let hold = self.hold.lock().unwrap().1;
        let agents = rows.iter().filter(|(k, ..)| k == "subagent").map(|(_, l, ..)| l.clone()).collect();
        let monitors = rows.iter().any(|r| r.5);
        let (fresh, stale): (Vec<_>, Vec<_>) =
            rows.iter().filter(|(k, ..)| k == "background").partition(|(_, _, since, ..)| now.saturating_sub(*since as u64) < hold);
        // Every command past the hold already named by a delivered notice: since the earliest of those notices.
        let earliest = |rows: &[&(String, String, i64, bool, Option<i64>, bool)]| {
            (!rows.is_empty()).then(|| rows.iter().map(|r| r.4).collect::<Option<Vec<i64>>>()).flatten().and_then(|n| n.into_iter().min()).map(|n| n as u64)
        };
        let repeat_all = earliest(&stale);
        // A monitor alone keeps no command's notification turn back: the turn after a download's "finished" while
        // its watch still runs is the result.
        let repeat = earliest(&stale.iter().copied().filter(|r| !r.5).collect::<Vec<_>>());
        // Past the hold, a command without an output file (recorded before the probe existed, or whose item never
        // named one) is unknown, not running (night pass 3, 21 Oct 04:00: four finished commands reported as running).
        let (stale, unchecked): (Vec<_>, Vec<_>) = stale.into_iter().partition(|r| r.3);
        let unchecked_recent = unchecked.iter().filter(|r| now.saturating_sub(r.2 as u64) < UNCHECKED_HOLD_MS).count();
        // Commands past the hold whose output file is gone (a cleaned /tmp, a path quoted wrongly): notices still name
        // them, but nothing can ever probe them, so they never keep an island on Background or a cleared task open for
        // good (`background_work`, `background_open`; fresh review, 4 Nov).
        let gone: Vec<i64> = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT output,since_ms FROM children WHERE thread=? AND kind='background' AND until_ms IS NULL AND output IS NOT NULL AND since_ms<=?")?
            .query_map(params![thread, now.saturating_sub(hold) as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .filter(|(path, _)| !std::path::Path::new(path).exists())
            .map(|(_, since)| since)
            .collect();
        Ok(Open {
            unchecked_recent,
            stale_gone: gone.len(),
            stale_gone_recent: gone.iter().filter(|since| now.saturating_sub(**since as u64) < UNCHECKED_HOLD_MS).count(),
            agents,
            commands: fresh.iter().map(|(_, l, ..)| l.clone()).collect(),
            stale: stale.iter().map(|(_, l, ..)| l.clone()).collect(),
            unchecked: unchecked.iter().map(|(_, l, ..)| l.clone()).collect(),
            repeat,
            repeat_all,
            monitors,
        })
    }
    /// Bug note 01-28-46: background commands that finished without a trace in T3's timeline. T3 writes a command's
    /// "finished" notification only when it starts a turn (one that finishes during a running turn reaches the model
    /// inside that turn and leaves no item), and quotes the command's description, not its command line ("Wait for
    /// Maya1 download"). So four finished downloads of the TTS research held its final reply for 30 minutes. Claude
    /// writes each background command's output to a file that the command's processes hold open while it runs: an
    /// existing output file nobody writes is a finished command. A missing file, or a probe that fails, proves nothing
    /// (the command stays open, `BACKGROUND_HOLD_MS` still bounds it). Commands past that hold are probed too (bug
    /// note 22-19-38: only those within it were, so a check run and a commit that ended at 21:47 and 21:57 were named
    /// "still running" in every notice of the voice-pipeline task until 22:35).
    async fn settle_background(&self, thread: &str) -> Result<()> {
        let rows: Vec<(String, String)> = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT id,output FROM children WHERE thread=? AND kind='background' AND output IS NOT NULL")?
            .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // Only files that exist: their real paths are what lsof reports.
        let files: Vec<(String, String)> = rows
            .into_iter()
            .filter_map(|(id, path)| std::fs::canonicalize(&path).ok().map(|real| (id, real.to_string_lossy().into_owned())))
            .collect();
        if files.is_empty() {
            return Ok(());
        }
        let Some(open) = writers(&files.iter().map(|(_, f)| f.clone()).collect::<Vec<_>>()).await else { return Ok(()) };
        for (id, file) in files.iter().filter(|(_, f)| !open.contains(f)) {
            eprintln!("t3: background command {id} of {thread} has finished (nothing writes {file})");
            self.db.lock().unwrap().execute("DELETE FROM children WHERE thread=? AND id=?", params![thread, id])?;
            self.db.lock().unwrap().execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, crate::now_ms() as i64])?;
        }
        Ok(())
    }
    /// `task_status`: every open task in one line each, or one task's state and latest reply in full.
    async fn task_status(&self, a: &Value) -> Result<String> {
        let Some(id) = a["taskId"].as_str() else {
            let rows: Vec<(String, Option<String>, String, String)> = self
                .db
                .lock()
                .unwrap()
                .prepare("SELECT thread,title,status,model FROM workers WHERE archived=0 AND kind='task' ORDER BY rowid DESC")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            if rows.is_empty() {
                return Ok("No open tasks.".into());
            }
            let mut lines = vec![];
            for (thread, title, status, model) in rows.iter().take(20) {
                let open = self.open_children(thread)?;
                let state = crate::text::task_state(status, open.agents.len(), open.commands.len());
                lines.push(crate::text::task_line(thread, title.as_deref(), &state, model));
            }
            let mut out = format!("taskId · title · state · model:\n{}", lines.join("\n"));
            if rows.len() > 20 {
                out.push_str(&format!("\n{} older open tasks not listed; close finished ones with task_cancel.", rows.len() - 20));
            }
            return Ok(out);
        };
        self.task(id)?;
        let l = self.latest(id, "messages", 1, 4000).await?;
        let status = l.thread["status"].as_str().unwrap_or("idle");
        let open = self.open_children(id)?;
        let labels = open.commands;
        let state = crate::text::task_state(status, open.agents.len(), labels.len());
        let t = &l.thread;
        let name = self.title(id, t["title"].as_str())?.unwrap_or_else(|| id.to_owned());
        let mut out = format!("'{}' · {state}", crate::text::title(&name));
        if let Some(ago) = t["updatedAt"].as_str().and_then(|u| crate::text::ago(u, crate::now_ms())) {
            out.push_str(&format!(" · updated {ago}"));
        }
        if let Some(m) = t["model"].as_str() {
            out.push_str(&format!(" · {m}"));
        }
        if crate::threads::asks(t) {
            out.push_str(" · waits on a question or approval");
        }
        out.push_str(&format!(" [task {id}]"));
        if !labels.is_empty() {
            out.push_str(&format!("\nBackground: {}", labels.join("; ")));
        }
        let Some(item) = l.items.last().filter(|i| i["type"] == "assistant_message").cloned() else {
            out.push_str("\nNo reply yet.");
            return Ok(out);
        };
        let (text, whole) = self.whole_text(id, item.clone(), REPLY_BYTES).await?;
        out.push_str(&format!("\n{}:\n{}", if state == "done" { "Final reply" } else { "Latest reply" }, text.trim()));
        if !whole {
            let offset = text.encode_utf16().count();
            out.push_str(&format!(
                "\nMore: t3_thread_read {{threadId: \"{id}\", itemId: \"{}\", view: \"messages\", textOffset: {offset}}}",
                item["itemId"].as_str().unwrap_or("")
            ));
        }
        Ok(out)
    }
    /// `task_cancel`: interrupt the task's running turn (its finish then wakes nothing), wait briefly for it to stop,
    /// and archive the thread.
    async fn task_cancel(&self, a: &Value, op: &str, permit: &DeliveryPermit) -> Result<String> {
        let id = a["taskId"].as_str().context("taskId missing")?;
        let w = self.task(id)?;
        if let Some(done) = self.prepare(op, "task_cancel", a, permit)? {
            return Ok(done.as_str().unwrap_or("Done.").to_owned());
        }
        self.quiet_for(id, 10 * 60 * 1000)?;
        let r = self.client.call("t3_thread_read", json!({"threadId":id,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?;
        let name = crate::text::title(r["thread"]["title"].as_str().unwrap_or(id));
        let mut stopped = false;
        if let Some(run) = r["thread"]["activeRunId"].as_str() {
            let mut args = json!({"threadId":id,"runId":run,"clientRequestId":op});
            if let Some(reason) = a["reason"].as_str() {
                args["reason"] = json!(reason);
            }
            permit.check()?;
            self.client.call("t3_thread_interrupt", args).await?;
            let waited = permit
                .run(self.client.call_deadline("t3_thread_wait", json!({"threadId":id,"runId":run,"timeoutMs":10_000}), Duration::from_secs(20)))
                .await?;
            if waited["timedOut"] == true {
                let text = format!("Interrupt requested for '{name}'; it has not stopped yet. Call task_cancel again to archive it once it has.");
                return Ok(text);
            }
            stopped = true;
        }
        if r["thread"]["archived"] != true {
            permit.check()?;
            self.client.call("t3_thread_organize", json!({"threadId":id,"action":"archive"})).await?;
        }
        self.db.lock().unwrap().execute("UPDATE workers SET archived=1 WHERE thread=?", [&w.thread])?;
        self.db.lock().unwrap().execute("DELETE FROM held WHERE thread=?", [&w.thread])?;
        let text = if stopped { format!("Stopped and archived '{name}'.") } else { format!("Archived '{name}'.") };
        self.finish(op, &Value::String(text.clone()))?;
        self.changed.notify_one();
        Ok(text)
    }
    /// The user closed the job island of `thread`: if it is one of Heimdall's open tasks, it is archived once finished
    /// (`archive_cleared`, now and on every pass). Returns whether it was marked. A running task is never stopped.
    pub fn clear_task(&self, thread: &str) -> Result<bool> {
        let db = self.db.lock().unwrap();
        let open: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM workers WHERE thread=? AND kind='task' AND archived=0)", [thread], |r| r.get(0))?;
        if open {
            db.execute("INSERT OR REPLACE INTO cleared VALUES(?,?)", params![thread, crate::now_ms() as i64])?;
        }
        Ok(open)
    }
    /// Archive every cleared task that is finished: its last turn ended and its result notice went out, no sub-agent
    /// or background command it waits on is open (the hold's own rule, `open_children`), and T3 shows no run,
    /// question or approval on it. A task still at work keeps its mark and is looked at again on the next pass; one
    /// thread T3 cannot read is skipped, not the rest. Returns the titles archived.
    pub async fn archive_cleared(&self) -> Result<Vec<String>> {
        let marks: Vec<(String, Option<String>, String, bool)> = {
            let db = self.db.lock().unwrap();
            // Marks of threads no longer open tasks (archived meanwhile, by task_cancel or by hand) go.
            db.execute("DELETE FROM cleared WHERE NOT EXISTS(SELECT 1 FROM workers WHERE workers.thread=cleared.thread AND kind='task' AND archived=0)", [])?;
            let mut q = db.prepare(
                "SELECT w.thread,w.run,w.status,
                   (NOT EXISTS(SELECT 1 FROM held h WHERE h.thread=w.thread) AND NOT EXISTS(SELECT 1 FROM pending_finals f WHERE f.thread=w.thread)
                    AND EXISTS(SELECT 1 FROM notices n WHERE n.thread=w.thread AND n.run=w.run AND n.kind IN ('terminal','quiet')))
                 FROM cleared JOIN workers w ON w.thread=cleared.thread",
            )?;
            let rows = q.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            rows
        };
        let mut out = vec![];
        for (thread, run, status, reported) in marks {
            if crate::threads::ACTIVE.contains(&status.as_str()) || !reported {
                continue;
            }
            match self.archive_if_finished(&thread, run.as_deref()).await {
                // Logged here for every caller: the worker pass's own archives left no line (bug note 2026-11-03T23-16-29).
                Ok(Some(title)) => {
                    eprintln!("t3: cleared task '{title}' ({thread}) archived: finished, reported, nothing of it still running");
                    out.push(title)
                }
                Ok(None) => {}
                Err(e) => eprintln!("t3: cleared task {thread} not archived yet: {e:#}"),
            }
        }
        if !out.is_empty() {
            self.changed.notify_one();
        }
        Ok(out)
    }
    /// One cleared task whose result went out at `run`: archived when nothing of it is still open.
    async fn archive_if_finished(&self, thread: &str, run: Option<&str>) -> Result<Option<String>> {
        // Its background commands first: one whose output file nobody writes any more has ended (bug note 01-28-46).
        if let Err(e) = self.settle_background(thread).await {
            eprintln!("t3: background of cleared task {thread} not checked: {e:#}");
        }
        if self.background_open(thread)? {
            return Ok(None);
        }
        let r = self.client.call("t3_thread_read", json!({"threadId":thread,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?;
        let t = &r["thread"];
        let status = t["status"].as_str().unwrap_or("");
        let newer = t["latestRunId"].as_str().is_some_and(|l| Some(l) != run);
        if !t["activeRunId"].is_null() || crate::threads::ACTIVE.contains(&status) || newer {
            return Ok(None);
        }
        // A question T3 still lists keeps it open; a finished run's leftover approval does not (bug note
        // 2026-11-02T14-56-51: the hook answered it, T3 never closed it).
        if t["pendingRequestCount"].as_u64().unwrap_or(0) > 0 && !crate::question_ids(&self.client, thread).await?.is_empty() {
            return Ok(None);
        }
        // A message Heimdall sent meanwhile (a steer, a follow-up) moved the task on: not archived.
        let same = |db: &Connection| -> Result<bool> {
            Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM workers WHERE thread=? AND run IS ? AND archived=0) AND EXISTS(SELECT 1 FROM cleared WHERE thread=?)", params![thread, run, thread], |r| r.get(0))?)
        };
        if !same(&self.db.lock().unwrap())? {
            return Ok(None);
        }
        if t["archived"] != true {
            self.client.call("t3_thread_organize", json!({"threadId":thread,"action":"archive"})).await?;
        }
        let db = self.db.lock().unwrap();
        // Only the run it was cleared at: a send that landed during the archive call keeps its new run followed.
        if db.execute("UPDATE workers SET archived=1 WHERE thread=? AND run IS ?", params![thread, run])? == 0 {
            return Ok(None);
        }
        db.execute("DELETE FROM held WHERE thread=?", [thread])?;
        db.execute("DELETE FROM cleared WHERE thread=?", [thread])?;
        Ok(Some(crate::text::title(t["title"].as_str().unwrap_or(thread))))
    }
    /// A command item cut at `ACTIVITY_CHARS` with its rest, up to `COMMAND_CHARS` more, in one read (bug note
    /// 2026-11-03T23-16-29: the Arsenal task's re-colour was started behind a 1,900-character heredoc, so its "Command
    /// running in background with ID" line came after the cut, the command was never counted, its turn's result went
    /// out at once and its cleared task was archived 58 s later, killing it). Only a background command's own result
    /// is taken: the read reaches the item's end and the marker line ends it (`background_tail`). A long foreground
    /// output that quotes the marker (a grep of a transcript) is left as it was. Recent items only (the caller), so a
    /// catch-up scan of old work costs no reads.
    async fn command_text(&self, thread: &str, item: &Value) -> Result<Value> {
        let Some(offset) = item["nextTextOffset"].as_u64() else { return Ok(item.clone()) };
        let args = json!({"threadId":thread,"view":"activity","itemId":item["itemId"],"textOffset":offset,"limit":1,"maxCharsPerItem":COMMAND_CHARS,"runLimit":1});
        let r = self.client.call("t3_thread_read", args).await.with_context(|| format!("rest of command item {} of {thread}", item["itemId"].as_str().unwrap_or("?")))?;
        let Some(rest) = r["items"].as_array().and_then(|a| a.first()).filter(|i| i["textTruncated"] != true) else { return Ok(item.clone()) };
        let text = format!("{}{}", item["text"].as_str().unwrap_or(""), rest["text"].as_str().unwrap_or(""));
        if !background_tail(&text) {
            return Ok(item.clone());
        }
        let mut whole = item.clone();
        whole["text"] = json!(text);
        whole["textTruncated"] = json!(false);
        Ok(whole)
    }
    /// Whether a thread still has work going after its turn (bug note 2026-11-03T23-16-29: a cleared task was archived,
    /// which ends its Claude session and every process it started, while its work ran): a sub-agent, a background
    /// command within the hold or past it with its output still written, a Monitor within its timeout, or a command
    /// nothing can check (no output file, or one that is gone) that started within `UNCHECKED_HOLD_MS`. Heimdall never archives a task while this holds.
    pub fn background_open(&self, thread: &str) -> Result<bool> {
        let open = self.open_children(thread)?;
        let stale_live = open.stale.len().saturating_sub(open.stale_gone);
        Ok(!open.agents.is_empty() || !open.commands.is_empty() || stale_live > 0 || open.monitors || open.unchecked_recent + open.stale_gone_recent > 0)
    }
    /// The background commands and monitors a followed thread still has running, by Heimdall's own record across all
    /// its runs (`children`): what its job island shows as Background after the turn ended (bug note
    /// 2026-11-03T22-49-42: the island read only the latest run's items and said finished while a restore ran).
    pub fn background_work(&self, thread: &str) -> Result<usize> {
        let open = self.open_children(thread)?;
        Ok(open.commands.len() + open.stale.len().saturating_sub(open.stale_gone))
    }
    /// Heimdall's own stop of a thread: its interrupted or archived ending is not news to Heimdall.
    pub(super) fn quiet_for(&self, thread: &str, ms: u64) -> Result<()> {
        self.db.lock().unwrap().execute("INSERT OR REPLACE INTO quiet VALUES(?,?)", params![thread, (crate::now_ms() + ms) as i64])?;
        Ok(())
    }
    /// Follow the run Heimdall just sent a message into on a thread it did not start, until that run's result.
    /// `cursor`: the thread's activity position before the send, so only what follows counts.
    pub(super) fn watch_run(&self, thread: &str, run: &str, status: &str, cursor: Option<u64>, title: Option<&str>) -> Result<()> {
        let db = self.db.lock().unwrap();
        let kind: Option<String> = db.query_row("SELECT kind FROM workers WHERE thread=?", [thread], |r| r.get(0)).optional()?;
        match kind.as_deref() {
            Some("task") => {
                db.execute("UPDATE workers SET run=?,status=? WHERE thread=?", params![run, status, thread])?;
            }
            Some(_) => {
                // Watched again: only what follows this message counts (its earlier watch ended with its result).
                db.execute(
                    "UPDATE workers SET run=?1,target_run=?1,status=?2,archived=0,activity_cursor=?3,messages_cursor=?3,trail_until=NULL,trail_run=NULL WHERE thread=?4",
                    params![run, status, cursor.map(|c| c as i64), thread],
                )?;
                db.execute("DELETE FROM children WHERE thread=?", [thread])?;
                db.execute("DELETE FROM subagents WHERE thread=?", [thread])?;
                db.execute("DELETE FROM run_starts WHERE thread=?", [thread])?;
                db.execute("DELETE FROM withheld WHERE thread=?", [thread])?;
            }
            None => {
                db.execute(
                    "INSERT INTO workers(thread,run,target_run,status,provider,family,model,project,path,title,kind,activity_cursor,messages_cursor) VALUES(?1,?2,?2,?3,'','','','','',?4,'watch',?5,?5)",
                    params![thread, run, status, title, cursor.map(|c| c as i64)],
                )?;
            }
        }
        db.execute("DELETE FROM held WHERE thread=?", [thread])?;
        Ok(())
    }
    /// A thread launched or forked through `t3_call` is a task like `delegate_task`'s.
    /// Returns the task hook's note (the job island it got), for the call's answer.
    pub(super) fn adopt(&self, thread: &str, run: Option<&str>, status: &str, selection: &Value, project: Option<&str>, title: Option<&str>, brief: &str) -> Result<String> {
        self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO workers(thread,run,status,provider,family,model,project,path,title,kind) VALUES(?1,?2,?3,?4,'',?5,?6,'',?7,'task')",
            params![thread, run, status, selection["instanceId"].as_str().unwrap_or(""), selection["model"].as_str().unwrap_or(""), project.unwrap_or(""), title],
        )?;
        self.run_started(thread, run, title.unwrap_or("Task"), brief)
    }
    async fn emit(
        &self,
        id: &str,
        w: &Worker,
        kind: &str,
        text: &str,
        permit: &DeliveryPermit,
    ) -> Result<()> {
        self.emit_with_short(id, w, kind, text, None, permit).await
    }
    /// `emit` with the short form the outbox sends instead if the agent rejects `text` as too long.
    async fn emit_with_short(
        &self,
        id: &str,
        w: &Worker,
        kind: &str,
        text: &str,
        short: Option<&str>,
        permit: &DeliveryPermit,
    ) -> Result<()> {
        permit.check()?;
        // Outbox first: a crash between tables can never lose delivery.
        self.actions.enqueue_with_short(id, text, short, permit)?;
        self.db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO notices VALUES(?1,?2,?3,?4)",
            params![id, w.thread, w.run, kind],
        )?;
        self.actions.drain(permit)
    }
    /// Retain every message item of the run in full; returns the thread's title and the run's last assistant reply,
    /// preceded by the last replies of the withheld turns in `carry` that came before it (`with_earlier`).
    async fn collect_final(&self, w: &Worker, carry: &[String], permit: &DeliveryPermit) -> Result<Final> {
        let mut last = Final::default();
        let Some(run) = &w.run else { return Ok(last) };
        let mut args = json!({"threadId":w.thread,"view":"messages","limit":1,"maxCharsPerItem":4000,"runLimit":1});
        // A watched thread can be long (the user's own): its run's messages are near the end.
        if let Some(start) = self.db.lock().unwrap().query_row("SELECT messages_cursor FROM workers WHERE thread=?", [&w.thread], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten() {
            args["afterPosition"] = json!(start);
        }
        let mut text = String::new();
        let mut tracked: Option<String> = None;
        let mut finished = false;
        // This run's first message, and each withheld run seen: its first message and last reply, in thread order.
        let mut first: Option<i64> = None;
        let mut earlier: Vec<(String, i64, Option<String>)> = vec![];
        for _ in 0..10000 {
            permit.check()?;
            // A read of the rest of one item (`itemId`) answers `hasMore: false` whatever follows it (night pass 3,
            // 21 Oct 04:00: a 6 KB brief ended the collection at the first item, so the notice had "no reply text").
            let one_item = args.get("itemId").is_some();
            let r = self.client.call("t3_thread_read", args.clone()).await?;
            if last.title.is_none() {
                last.title = r["thread"]["title"].as_str().map(str::to_owned);
            }
            let Some(item) = r["items"].as_array().and_then(|a| a.first()) else {
                finished = true;
                break;
            };
            let of = item["runId"].as_str().unwrap_or("");
            let mine = of == run.as_str();
            let carried = !mine && carry.iter().any(|c| c == of);
            if mine || carried {
                if tracked.as_deref() != item["itemId"].as_str() {
                    text.clear();
                }
                tracked = item["itemId"].as_str().map(str::to_owned);
                text.push_str(item["text"].as_str().unwrap_or(""));
                ensure!(text.len() <= 16 * 1024 * 1024, "T3 final item exceeds retention bound; inspect the task manually");
            }
            if item["textTruncated"] == true {
                args["itemId"] = item["itemId"].clone();
                let next = item["nextTextOffset"].as_u64().context("truncated final result has no offset")?;
                ensure!(args["textOffset"].as_u64().is_none_or(|n| next > n), "T3 text cursor made no progress");
                args["textOffset"] = json!(next);
                continue;
            }
            let position = item["position"].as_i64().unwrap_or(i64::MAX);
            if mine {
                first.get_or_insert(position);
                self.db.lock().unwrap().execute(
                    "INSERT OR REPLACE INTO final_items VALUES(?1,?2,?3,?4)",
                    params![w.thread, run, item["itemId"].as_str(), text],
                )?;
                if item["type"] == "assistant_message" {
                    last.reply = Some(text.clone());
                }
            } else if carried {
                let at = match earlier.iter().position(|(r, _, _)| r == of) {
                    Some(at) => at,
                    None => {
                        earlier.push((of.to_owned(), position, None));
                        earlier.len() - 1
                    }
                };
                if item["type"] == "assistant_message" {
                    earlier[at].2 = Some(text.clone());
                }
            }
            text.clear();
            tracked = None;
            args.as_object_mut().unwrap().remove("itemId");
            args.as_object_mut().unwrap().remove("textOffset");
            args["afterPosition"] = item["position"].clone();
            if !one_item && r["hasMore"] != true {
                finished = true;
                break;
            }
        }
        ensure!(finished, "T3_FINAL_INCOMPLETE: collection bound reached; read the task before accepting it");
        // Carried: withheld turns before this run (a later one waits for its own successor's notice), and any without
        // a message at all (a queued turn cancelled before it started).
        let before = |p: i64| first.is_none_or(|f| p < f);
        last.carried = carry.iter().filter(|c| earlier.iter().find(|(r, _, _)| r == *c).is_none_or(|(_, p, _)| before(*p))).cloned().collect();
        let replies: Vec<String> = earlier.into_iter().filter(|(_, p, _)| before(*p)).filter_map(|(_, _, reply)| reply).collect();
        last.own = last.reply.clone();
        last.reply = with_earlier(&replies, last.reply);
        Ok(last)
    }
    /// The finished notice of a run, once. `still`: what it still waits on (`text::still`: sub-agents past
    /// `SUBAGENT_HOLD_MS`, background work past `BACKGROUND_HOLD_MS`). `keep`: a watched thread stays followed after
    /// the notice (its sub-agents still run, so the turn that reads them is still the answer to Heimdall's message).
    async fn terminal_notice(&self, w: &Worker, state: &str, still: Option<&str>, keep: bool, permit: &DeliveryPermit) -> Result<()> {
        self.terminal_notice_as(w, state, still, keep, None, permit).await
    }
    /// `terminal_notice` under its own id (`key`, the heartbeat's second delivery) or the run's usual one.
    async fn terminal_notice_as(&self, w: &Worker, state: &str, still: Option<&str>, keep: bool, key: Option<String>, permit: &DeliveryPermit) -> Result<()> {
        let Some(run) = &w.run else { return Ok(()) };
        let usual = key.is_none();
        let key = key.unwrap_or_else(|| format!("terminal:{}:{run}:{state}", w.thread));
        // A sign-in failure waiting for its retry: withheld, its reply rides in the task's next notice.
        let waits: Option<String> = self.db.lock().unwrap().query_row("SELECT kind FROM notices WHERE id=?", [&key], |r| r.get(0)).optional()?;
        if waits.as_deref() == Some("auth-retry") {
            return Ok(());
        }
        let kind = self.kind(&w.thread)?;
        // Heimdall's own interrupt or archive (task_cancel, t3_call): the ending is not news (day review T3).
        if matches!(state, "interrupted" | "cancelled") && self.quiet(&w.thread)? {
            self.db.lock().unwrap().execute("INSERT OR IGNORE INTO notices VALUES(?1,?2,?3,'quiet')", params![key, w.thread, run])?;
            self.db.lock().unwrap().execute("DELETE FROM withheld WHERE thread=? AND run=?", params![w.thread, run])?;
            self.unwatch(&w.thread, &kind)?;
            return Ok(());
        }
        self.db.lock().unwrap().execute("INSERT OR IGNORE INTO pending_finals VALUES(?,?,?)", params![w.thread, run, state])?;
        let delivered = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM notices WHERE id=?)", [&key], |r| r.get::<_, bool>(0))?;
        let mut carried = vec![];
        let mut emitted = Ok(());
        if !delivered {
            // Earlier turns of this thread that a later one superseded without a notice: their replies come first.
            let carry: Vec<String> = self
                .db
                .lock()
                .unwrap()
                .prepare("SELECT run FROM withheld WHERE thread=?1 AND run!=?2")?
                .query_map(params![w.thread, run], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let last = permit.run(self.collect_final(w, &carry, permit)).await?;
            let mut still = still.map(str::to_owned);
            if kind == "task" && usual {
                match self.auth_episode(&w.thread, run, state, last.own.as_deref())? {
                    AuthStep::Retry => {
                        // Not news yet: the one automatic retry goes out at `due_ms` (`auth_retries`); this turn's reply
                        // (and the earlier ones it would have carried) ride in the next notice.
                        self.db.lock().unwrap().execute("INSERT OR IGNORE INTO notices VALUES(?1,?2,?3,'auth-retry')", params![key, w.thread, run])?;
                        self.db.lock().unwrap().execute("INSERT OR IGNORE INTO withheld VALUES(?,?)", params![w.thread, run])?;
                        self.db.lock().unwrap().execute("DELETE FROM pending_finals WHERE thread=? AND run=?", params![w.thread, run])?;
                        return Ok(());
                    }
                    AuthStep::Report(line) => still = Some(still.map_or(line.clone(), |s| format!("{s} {line}"))),
                    AuthStep::None => {}
                }
            }
            let still = still.as_deref();
            carried = last.carried.clone();
            // The reply itself, whole: Heimdall never needs a read just to see the answer.
            let title = self.title(&w.thread, last.title.as_deref())?;
            let budget = self.actions.notice_budget();
            let (text, short) = if kind == "watch" {
                (
                    crate::text::watched(&w.thread, title.as_deref(), state, last.reply.as_deref(), budget, still),
                    crate::text::watched(&w.thread, title.as_deref(), state, last.reply.as_deref(), 0, still),
                )
            } else {
                (
                    crate::text::finished_with(&w.thread, title.as_deref(), state, last.reply.as_deref(), budget, still),
                    crate::text::finished_with(&w.thread, title.as_deref(), state, last.reply.as_deref(), 0, still),
                )
            };
            // Written to the outbox and recorded as sent before the delivery is tried: a delivery that fails now goes
            // out from the outbox later. The turns it carries are retired once it is delivered: at once, or by
            // `retire_carried` when the outbox (or the heartbeat's resend, which carries them again) delivers it
            // (night pass 3, 21 Oct 04:00:52: an unacknowledged delivery left runs 1-24 of the night pass in
            // `withheld` for good).
            emitted = self.emit_with_short(&key, w, "terminal", &text, Some(&short), permit).await;
            let recorded = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM notices WHERE id=?)", [&key], |r| r.get::<_, bool>(0))?;
            if !recorded {
                return emitted;
            }
            // The background commands this notice names as still running (or unchecked): Heimdall has heard of them,
            // so later turns that wait only on them are progress (`repeat`, bug note 22-19-38).
            if let Some(still) = still {
                let db = self.db.lock().unwrap();
                let rows: Vec<(String, String)> = db
                    .prepare("SELECT id,label FROM children WHERE thread=? AND kind='background'")?
                    .query_map([&w.thread], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?;
                // Whole items of the notice's lists only (": a; b."), so a short label inside a longer one is not named.
                let named = |label: &str| [": ", "; "].iter().any(|a| [";", "."].iter().any(|b| still.contains(&format!("{a}{label}{b}"))));
                for (id, label) in rows.iter().filter(|(_, label)| !label.is_empty() && named(label)) {
                    db.execute("UPDATE children SET noticed_ms=? WHERE thread=? AND id=?", params![crate::now_ms() as i64, w.thread, id])?;
                    if label.starts_with("monitor \"") {
                        db.execute("INSERT OR REPLACE INTO watch_notices VALUES(?,?,?)", params![w.thread, label, crate::now_ms() as i64])?;
                    }
                }
            }
            if emitted.is_err() {
                let db = self.db.lock().unwrap();
                for done in &carried {
                    db.execute("INSERT OR IGNORE INTO carried VALUES(?1,?2,?3)", params![key, w.thread, done])?;
                }
                carried.clear();
            }
        }
        for done in carried.iter().chain([run]) {
            self.db.lock().unwrap().execute("DELETE FROM withheld WHERE thread=? AND run=?", params![w.thread, done])?;
        }
        self.db.lock().unwrap().execute("DELETE FROM pending_finals WHERE thread=? AND run=?", params![w.thread, run])?;
        self.actions.monitor_failure(&format!("{}:{run}", w.thread), false)?;
        if !keep && kind == "watch" {
            // Still followed for the thread's own follow-up runs (`TRAIL_MS`); a notice sent now restarts the time.
            let until = (crate::now_ms() + *self.trail.lock().unwrap()) as i64;
            self.db.lock().unwrap().execute(
                "UPDATE workers SET trail_until=?1,trail_run=?2 WHERE thread=?3 AND kind='watch' AND (?4 OR trail_until IS NULL)",
                params![until, run, w.thread, !delivered],
            )?;
        }
        emitted
    }
    /// Bug note 2026-10-24T23-34-21: what a task's finished turn means for a sign-in episode. A turn failing on its
    /// provider's sign-in (`text::auth_failure`) starts one: not reported, one automatic retry after `AUTH_RETRY_MS`;
    /// more such failures before that retry are the same episode. One after the retry is reported, saying the retry
    /// failed too and that more messages fail the same way. A turn that ends any other way ends the episode.
    fn auth_episode(&self, thread: &str, run: &str, state: &str, reply: Option<&str>) -> Result<AuthStep> {
        let now = crate::now_ms();
        let db = self.db.lock().unwrap();
        let row: Option<(i64, Option<i64>)> = db.query_row("SELECT failed_ms,retried_ms FROM auth_failures WHERE thread=?", [thread], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        // Only the turn's own last words, their first line: a progress reply quoting an API's "401 Unauthorized" is not
        // a sign-in failure of the task (review 27 Oct).
        let first = reply.and_then(|r| r.lines().map(str::trim).find(|l| !l.is_empty()));
        let Some(reason) = first.filter(|_| state == "failed").and_then(crate::text::auth_failure) else {
            if terminal(state) {
                db.execute("DELETE FROM auth_failures WHERE thread=?", [thread])?;
            }
            return Ok(AuthStep::None);
        };
        match row {
            Some((failed, retried)) if now.saturating_sub(failed as u64) < AUTH_EPISODE_MS => match retried {
                // Another turn failing the same way before the retry (T3's own turns fail at once, mcp:14c20ff4 runs 21-29):
                // the retry follows the latest.
                None => {
                    db.execute("UPDATE auth_failures SET run=? WHERE thread=?", params![run, thread])?;
                    Ok(AuthStep::Retry)
                }
                Some(at) => {
                    db.execute("UPDATE auth_failures SET failed_ms=?,run=? WHERE thread=?", params![now as i64, run, thread])?;
                    let ago = now.saturating_sub(at as u64) / 60_000;
                    Ok(AuthStep::Report(format!(
                        "It failed on its provider's sign-in again ({reason}) after Heimdall's one automatic retry {ago} min ago; every message to it fails the same way until that sign-in works again (the provider signed in again in T3), so send it nothing until then."
                    )))
                }
            },
            _ => {
                let due = now + *self.auth_retry.lock().unwrap();
                db.execute(
                    "INSERT OR REPLACE INTO auth_failures(thread,run,failed_ms,due_ms,retried_ms,reason) VALUES(?,?,?,?,NULL,?)",
                    params![thread, run, now as i64, due as i64, reason],
                )?;
                eprintln!("t3: {thread} run {run} failed on its sign-in ({reason}); one automatic retry in {} s", (due - now) / 1000);
                Ok(AuthStep::Retry)
            }
        }
    }
    /// The automatic retries that are due: one message each, idempotent by the failed run (`clientRequestId`). A task
    /// that is working again (a turn of its own started) needs none. A send that fails is tried on the next pass, and
    /// after `AUTH_EPISODE_MS` the failure is reported instead.
    async fn auth_retries(&self, permit: &DeliveryPermit) -> Result<()> {
        let now = crate::now_ms();
        let due: Vec<(String, String, i64)> = self
            .db
            .lock()
            .unwrap()
            // Due by the current `AUTH_RETRY_MS` after the first failure of the episode (tests shorten it).
            .prepare("SELECT thread,run,failed_ms FROM auth_failures WHERE retried_ms IS NULL AND failed_ms+?<=?")?
            .query_map(params![*self.auth_retry.lock().unwrap() as i64, now as i64], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (thread, run, failed) in due {
            let current: Option<(Option<String>, String)> = self.db.lock().unwrap().query_row("SELECT run,status FROM workers WHERE thread=? AND kind='task' AND archived=0", [&thread], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            // Archived, or working again by itself (a later turn started or ended): no retry, the episode is over.
            if !current.as_ref().is_some_and(|(r, status)| r.as_deref() == Some(run.as_str()) && !crate::threads::ACTIVE.contains(&status.as_str())) {
                self.db.lock().unwrap().execute("DELETE FROM auth_failures WHERE thread=?", [&thread])?;
                continue;
            }
            let key: String = run.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect();
            let args = json!({"threadId": thread, "message": AUTH_RETRY_MESSAGE, "mode": "auto", "clientRequestId": format!("heimdall-auth-retry-{key}")});
            match permit.run(self.client.call("t3_thread_send", args)).await {
                Ok(r) => {
                    eprintln!("t3: {thread} retried once after its sign-in failure");
                    let db = self.db.lock().unwrap();
                    db.execute("UPDATE auth_failures SET retried_ms=? WHERE thread=?", params![now as i64, thread])?;
                    if let Some(next) = r["runId"].as_str() {
                        db.execute("UPDATE workers SET run=?,status=? WHERE thread=? AND kind='task'", params![next, r["status"].as_str().unwrap_or("running"), thread])?;
                    }
                }
                Err(e) if now.saturating_sub(failed as u64) < AUTH_EPISODE_MS => eprintln!("t3: the automatic retry of {thread} did not go out ({e:#}); trying again on the next pass"),
                Err(e) => {
                    // Too long: the failure itself is the news.
                    self.db.lock().unwrap().execute("UPDATE auth_failures SET retried_ms=? WHERE thread=?", params![now as i64, thread])?;
                    let w = Worker { thread: thread.clone(), run: Some(run.clone()), status: "failed".into() };
                    let line = format!("It failed on its provider's sign-in and Heimdall's automatic retry could not be sent ({e:#}).");
                    self.terminal_notice_as(&w, "failed", Some(&line), false, Some(format!("terminal:{thread}:{run}:failed:auth")), permit).await?;
                }
            }
        }
        Ok(())
    }
    /// Bug note 2026-10-24T23-42-04: whether Heimdall's send to `thread` should wait (its turns fail on the sign-in),
    /// as the answer that says so.
    fn auth_cooldown(&self, thread: &str) -> Result<Option<String>> {
        let row: Option<(i64, Option<i64>, String)> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT failed_ms,retried_ms,reason FROM auth_failures WHERE thread=?", [thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        let now = crate::now_ms();
        let cooldown = *self.auth_cooldown_ms.lock().unwrap();
        Ok(match row {
            // Bounded: a retry that cannot be sent (T3 refusing it) never keeps Heimdall's messages out for long
            // (review 27 Oct); past the cooldown after it was due, a send goes through.
            Some((failed, None, reason)) if now < (failed as u64).saturating_add(*self.auth_retry.lock().unwrap()).saturating_add(cooldown) => Some(format!(
                "NOT_SENT: thread {thread}'s last turn failed on its sign-in ({reason}); Heimdall retries it once by itself in {} min, so this message would only stack behind the same failure. Nothing to send now.",
                (failed as u64).saturating_add(*self.auth_retry.lock().unwrap()).saturating_sub(now).div_ceil(60_000)
            )),
            Some((failed, Some(retried), reason)) if failed > retried && now.saturating_sub(failed as u64) < cooldown => Some(format!(
                "NOT_SENT: thread {thread} failed on its sign-in again after Heimdall's automatic retry ({reason}); a message now fails the same way. Send after {} min, once its provider is signed in again in T3.",
                (failed as u64 + cooldown).saturating_sub(now).div_ceil(60_000)
            )),
            _ => None,
        })
    }
    /// Bug note 2026-10-24T23-42-04 (seven of Heimdall's messages stacked on one worker): a send to a thread where
    /// Heimdall's own earlier message still waits in T3's queue joins that message (`t3_queue` edit, the same queued
    /// run, followed as before) instead of queuing another turn. Entries T3 no longer lists are forgotten.
    async fn join_queued(&self, thread: &str, message: &str, permit: &DeliveryPermit) -> Result<Option<String>> {
        let mine: Vec<(String, String)> = self.db.lock().unwrap().prepare("SELECT run,text FROM queued_sends WHERE thread=? ORDER BY rowid")?.query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        if mine.is_empty() || message.trim().is_empty() {
            return Ok(None);
        }
        // A T3 without the queue tools (or a failed read): the message goes out as its own send.
        let list = match permit.run(self.client.call("t3_queue", json!({"action": "list", "threadId": thread, "limit": 100}))).await {
            Ok(list) => list,
            Err(e) => {
                eprintln!("t3: queue of {thread} not read ({e:#}); sending on its own");
                return Ok(None);
            }
        };
        let queued: Vec<&str> = list["items"].as_array().into_iter().flatten().filter_map(|i| i["queuedRunId"].as_str()).collect();
        let queued_text = |run: &str| list["items"].as_array().into_iter().flatten().find(|i| i["queuedRunId"] == run).and_then(|i| i["text"].as_str()).map(str::to_owned);
        for (run, _) in mine.iter().filter(|(run, _)| !queued.contains(&run.as_str())) {
            self.db.lock().unwrap().execute("DELETE FROM queued_sends WHERE thread=? AND run=?", params![thread, run])?;
        }
        let Some((run, text)) = mine.into_iter().rev().find(|(run, _)| queued.contains(&run.as_str())) else { return Ok(None) };
        let joined = format!("{}\n\n{}", text.trim_end(), message.trim());
        // Already joined (Heimdall stopped between the edit and recording it, review 27 Oct): T3's queued text ends
        // with this message, so nothing is added twice.
        let already = queued_text(&run).is_some_and(|t| t.trim_end().ends_with(&format!("\n\n{}", message.trim())));
        if already {
            self.db.lock().unwrap().execute("UPDATE queued_sends SET text=? WHERE thread=? AND run=?", params![queued_text(&run).unwrap_or(joined.clone()), thread, run])?;
        } else if let Err(e) = permit.run(self.client.call("t3_queue", json!({"action": "edit", "threadId": thread, "queuedRunId": run, "text": joined}))).await {
            // Started meanwhile (T3 rejects runs no longer queued): a send of its own.
            eprintln!("t3: queued run {run} of {thread} not joined ({e:#}); sending on its own");
            self.db.lock().unwrap().execute("DELETE FROM queued_sends WHERE thread=? AND run=?", params![thread, run])?;
            return Ok(None);
        }
        self.db.lock().unwrap().execute("UPDATE queued_sends SET text=? WHERE thread=? AND run=?", params![joined, thread, run])?;
        Ok(Some(format!(
            "Added to your message still queued on thread {thread} (queued run {run}): one turn reads both, and its reply arrives as a notice when it ends."
        )))
    }
    /// A watched thread is followed for one run: after its result, the thread is the user's again.
    fn unwatch(&self, thread: &str, kind: &str) -> Result<()> {
        if kind == "watch" {
            self.db.lock().unwrap().execute("UPDATE workers SET archived=1,trail_until=NULL,trail_run=NULL WHERE thread=? AND kind='watch'", [thread])?;
            self.db.lock().unwrap().execute("DELETE FROM children WHERE thread=?", [thread])?;
            self.db.lock().unwrap().execute("DELETE FROM subagents WHERE thread=?", [thread])?;
            self.db.lock().unwrap().execute("DELETE FROM run_starts WHERE thread=?", [thread])?;
            self.db.lock().unwrap().execute("DELETE FROM withheld WHERE thread=?", [thread])?;
        }
        Ok(())
    }
    async fn retry_finals(&self,permit:&DeliveryPermit)->Result<()> {
        let rows=self.db.lock().unwrap().prepare("SELECT thread,run,status FROM pending_finals")?.query_map([],|r|Ok(Worker{thread:r.get(0)?,run:r.get(1)?,status:r.get(2)?}))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for w in rows {self.terminal_notice(&w,&w.status,None,false,permit).await?;}Ok(())
    }
    /// What one activity item says about the work a thread waits on: a sub-agent opening or closing, a command sent
    /// to the background, its "finished" notification, or the agent killing it. Inherited items (a fork's history)
    /// are not this thread's work.
    fn track(&self, thread: &str, item: &Value) -> Result<()> {
        if item["visibility"].as_str().is_some_and(|v| v != "local") {
            return Ok(());
        }
        let id = item["itemId"].as_str().unwrap_or("");
        let text = item["text"].as_str().unwrap_or("");
        let now = crate::now_ms() as i64;
        let db = self.db.lock().unwrap();
        match item["type"].as_str().unwrap_or("") {
            "subagent" if !id.is_empty() => {
                let label = item["title"].as_str().unwrap_or("sub-agent");
                db.execute("INSERT OR IGNORE INTO subagents(thread,item,label,position) VALUES(?1,?2,?3,?4)", params![thread, id, label, item["position"].as_i64()])?;
                if open(item["status"].as_str().unwrap_or("")) {
                    db.execute("INSERT OR IGNORE INTO children(thread,id,kind,label,since_ms) VALUES(?1,?2,'subagent',?3,?4)", params![thread, id, label, now])?;
                } else if db.execute("DELETE FROM children WHERE thread=? AND id=?", params![thread, id])? > 0 {
                    db.execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, now])?;
                }
            }
            "dynamic_tool" if monitor_tool(text) => {
                // A Claude Monitor (`{"toolName":"Monitor","input":{description,timeout_ms,persistent},"output":
                // {"taskId":…}}`): each event it prints starts a turn ("Background activity updated"), so it is
                // background work the task waits on, until T3's `Monitor "…" was stopped`, a TaskStop of its id, or
                // its own timeout.
                let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                if let Some(task) = v["output"]["taskId"].as_str().filter(|t| !t.is_empty()) {
                    let what = v["input"]["description"].as_str().unwrap_or("background watch");
                    let label = crate::client::bounded_text(&format!("monitor \"{}\"", flat(what)), 120);
                    // A persistent monitor has no timeout: it counts for `SUBAGENT_HOLD_MS` at most.
                    let lasts = if v["input"]["persistent"] == true { SUBAGENT_HOLD_MS as i64 } else { v["input"]["timeout_ms"].as_f64().map_or(MONITOR_TIMEOUT_MS.0, |t| t as i64).min(MONITOR_TIMEOUT_MS.1) };
                    // The same watch re-armed (its description again) is not news when a notice named it before
                    // (mcp:b2d9d14a re-armed "Drama BF16 download progress, …" as each one timed out).
                    db.execute(
                        "INSERT OR IGNORE INTO children(thread,id,kind,label,since_ms,until_ms,noticed_ms)
                         VALUES(?1,?2,'background',?3,?4,?5,(SELECT noticed_ms FROM watch_notices WHERE thread=?1 AND label=?3))",
                        params![thread, task, label, now, now.saturating_add(lasts)],
                    )?;
                }
            }
            "notification" if quoted_kinds(text).iter().any(|(lead, _)| lead == "monitor") => {
                // `Monitor "A" was stopped`, `Monitor "A" and monitor "B" were stopped`, `Monitor "A" and command "x"
                // finished`: each named monitor or command closes.
                for (lead, name) in quoted_kinds(text) {
                    let monitor = lead == "monitor";
                    if lead == "subagent" {
                        // `Subagent "A" and monitor "B" finished` (review 27 Oct).
                        if db.execute("DELETE FROM children WHERE thread=? AND kind='subagent' AND label=?", params![thread, name])? > 0 {
                            db.execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, now])?;
                        }
                        continue;
                    }
                    if !monitor && lead != "command" {
                        continue;
                    }
                    let rows: Vec<(String, String)> = db
                        .prepare("SELECT id,label FROM children WHERE thread=? AND kind='background' AND (until_ms IS NOT NULL)=? ORDER BY since_ms")?
                        .query_map(params![thread, monitor], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?;
                    let label_of = |l: &str| if monitor { l.strip_prefix("monitor \"").map(|l| flat(l.trim_end_matches('"').trim_end_matches('…'))) } else { Some(flat(l)) };
                    let named = |label: &str| label_of(label).is_some_and(|l| !name.is_empty() && (l.starts_with(&name) || name.starts_with(&l)));
                    if let Some((done, _)) = rows.iter().find(|(_, l)| named(l)) {
                        db.execute("DELETE FROM children WHERE thread=? AND id=?", params![thread, done])?;
                        if !monitor {
                            db.execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, now])?;
                        }
                    }
                }
            }
            "notification" => {
                if let Some(prefix) = quoted(text, "Command ") {
                    // The oldest open background command whose command line starts with the quoted one.
                    let open: Vec<(String, String)> = db
                        .prepare("SELECT id,label FROM children WHERE thread=? AND kind='background' ORDER BY since_ms")?
                        .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
                        .collect::<rusqlite::Result<_>>()?;
                    let prefix = flat(&prefix);
                    if let Some((done, _)) = open.iter().find(|(_, l)| flat(l).starts_with(&prefix) || prefix.starts_with(&flat(l))) {
                        db.execute("DELETE FROM children WHERE thread=? AND id=?", params![thread, done])?;
                        db.execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, now])?;
                    }
                } else {
                    // `Subagent "A" finished`, or several at once: `Subagent "A" and subagent "B" finished`.
                    for name in subagent_names(text) {
                        let ids: Vec<String> = db
                            .prepare("SELECT id FROM children WHERE thread=? AND kind='subagent' AND label=?")?
                            .query_map(params![thread, name], |r| r.get(0))?
                            .collect::<rusqlite::Result<_>>()?;
                        for done in ids {
                            db.execute("DELETE FROM children WHERE thread=? AND id=?", params![thread, done])?;
                            db.execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, now])?;
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(start) = text.find(BACKGROUND_MARK) {
            let bg: String = text[start + BACKGROUND_MARK.len()..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            if !bg.is_empty() {
                let command = text[..start].trim().strip_prefix("$ ").unwrap_or(text[..start].trim());
                let label = crate::client::bounded_text(&flat(command), 120);
                db.execute(
                    "INSERT OR IGNORE INTO children(thread,id,kind,label,since_ms,output) VALUES(?1,?2,'background',?3,?4,?5)",
                    params![thread, bg, label, now, output_file(&text[start..])],
                )?;
            }
        } else if item["type"] == "dynamic_tool" && ["KillShell", "KillBash", "TaskStop"].iter().any(|k| text.contains(k)) {
            let ids: Vec<(String, bool)> = db
                .prepare("SELECT id,until_ms IS NOT NULL FROM children WHERE thread=? AND kind='background'")?
                .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (bg, monitor) in ids.iter().filter(|(bg, _)| text.contains(bg.as_str())) {
                db.execute("DELETE FROM children WHERE thread=? AND id=?", params![thread, bg])?;
                if !monitor {
                    db.execute("INSERT OR REPLACE INTO closes VALUES(?,?)", params![thread, now])?;
                }
            }
        }
        Ok(())
    }
    /// Whether the run after a finished turn was started by a message (someone's request, whose turn the earlier
    /// result does not wait for) or by T3 itself (its first input a hand-over notification, `t3_handover`: the turn
    /// that reads finished sub-agents or background work, so the earlier turn was progress). Any other start (a user
    /// message, a pull-request or command notification) is a message. None while that run has neither started nor shown an item, within `CLEAR_GRACE_MS` of the
    /// earlier turn's end (`ended`): decide on a later pass. Otherwise a run without any item seen counts as a message
    /// (the behaviour before 21 Oct; a queue the user paused never holds a result back for long).
    fn started_by_message(&self, thread: &str, next: &Value, ended: Option<&str>) -> Result<Option<bool>> {
        let run = next["runId"].as_str().unwrap_or("");
        let first: Option<String> = self.db.lock().unwrap().query_row("SELECT kind FROM run_starts WHERE thread=? AND run=?", params![thread, run], |r| r.get(0)).optional()?;
        let grace = self.hold.lock().unwrap().0 as i64 * 1_000_000;
        let now = crate::now_ms() as i64 * 1_000_000;
        Ok(match first {
            Some(kind) => Some(!matches!(kind.as_str(), "t3" | "activity")),
            None if next["startedAt"].is_null() && ended.and_then(at).is_some_and(|e| (now - e).max(0) < grace) => None,
            None => Some(true),
        })
    }
    /// The turns from `from` up to the thread's `current` run (by ordinal) that ended without a notice of their own:
    /// withheld, their replies carried by the thread's next notice (`terminal_notice`). `from` counts even when it is
    /// no longer among the recent runs.
    fn withhold(&self, thread: &str, runs: &[Value], from: &str, current: Option<&str>) -> Result<()> {
        let ordinal = |id: &str| runs.iter().find(|x| x["runId"] == id).and_then(|x| x["ordinal"].as_i64());
        let (start, end) = (ordinal(from), current.and_then(ordinal));
        let mut ids: Vec<&str> = runs
            .iter()
            .filter(|x| x["runId"].as_str() != current && x["status"].as_str().is_some_and(terminal))
            .filter(|x| x["ordinal"].as_i64().is_some_and(|o| start.is_none_or(|s| o >= s) && end.is_none_or(|e| o < e)))
            .filter_map(|x| x["runId"].as_str())
            .collect();
        if start.is_none() {
            ids.insert(0, from);
        }
        let db = self.db.lock().unwrap();
        for id in ids {
            db.execute(
                "INSERT OR IGNORE INTO withheld SELECT ?1,?2 WHERE NOT EXISTS(SELECT 1 FROM notices WHERE thread=?1 AND run=?2 AND kind IN ('terminal','quiet'))",
                params![thread, id],
            )?;
        }
        Ok(())
    }
    /// A finished turn's sub-agents, re-read before it counts as the result. Claude's sub-agent items close in place,
    /// and a SendMessage to a finished one re-opens the same item at its old position, under the current run (launch
    /// research, 20 Oct 19:20: "Resuming agent a3eb6be"), where the activity cursor never sees it again. Returns
    /// whether one is open again (it is back among the children) and whether one closed while this turn ran without
    /// the turn waiting for it: a background sub-agent (launched by an earlier turn, or one the parent worked beside),
    /// whose result T3 hands to its next turn (decider research, 20 Oct 19:18: the turn ended "still waiting on the
    /// vision/omni model sweep" 3 s after that sweep had closed). A sub-agent the turn waited for is its own business.
    async fn recheck_subagents(&self, thread: &str, run: &str, started: Option<i64>) -> Result<(bool, bool)> {
        let rows: Vec<(String, Option<i64>)> = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT item,position FROM subagents WHERE thread=?1 AND NOT EXISTS(SELECT 1 FROM children WHERE children.thread=?1 AND children.id=subagents.item) ORDER BY rowid DESC LIMIT 20")?
            .query_map([thread], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let (mut reopened, mut during) = (false, false);
        for (id, position) in rows {
            let r = self.client.call("t3_thread_read", json!({"threadId":thread,"view":"activity","itemId":id,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?;
            let Some(item) = r["items"].as_array().and_then(|i| i.first()) else {
                self.db.lock().unwrap().execute("DELETE FROM subagents WHERE thread=? AND item=?", params![thread, id])?;
                continue;
            };
            if open(item["status"].as_str().unwrap_or("")) {
                self.track(thread, item)?;
                reopened = true;
                continue;
            }
            let Some(closed) = item["updatedAt"].as_str().and_then(at) else { continue };
            // Closed by the time this turn started (T3 starts the turn that reads a sub-agent the moment it closes).
            if during || started.is_none_or(|s| closed <= s) {
                continue;
            }
            // Closed during this turn. Launched by an earlier turn: background. Launched by this one: background
            // when the parent went on working while it ran (an item after it, not a sub-agent, finished before it).
            let after = position.or(item["position"].as_i64());
            let (beside, read) = self.read_in_turn(thread, run, after, closed).await?;
            // Unless the turn took its result in itself: then T3 starts no turn to hand it over (bug note 22 Oct
            // 15:35: the Voxtral research's Explore agent closed 4 minutes before its turn ended; the result waited
            // out the grace for a hand-over that never comes).
            during = (item["runId"] != run || beside) && !read;
        }
        Ok((reopened, during))
    }
    /// After a sub-agent at `after` that closed at `closed` (ns) during the turn `run`: whether the parent worked beside
    /// it (the first item after it, not a sub-agent, finished before it closed), and whether the turn read its result
    /// itself: a tool call of this turn finished after the sub-agent closed and a model call followed it. Claude hands a
    /// finished background sub-agent to the next model call of a running turn; only one that closes while the turn's
    /// last model call runs (20 Oct 19:18, 3 s before the turn ended) is handed over in a turn T3 starts for it.
    async fn read_in_turn(&self, thread: &str, run: &str, after: Option<i64>, closed: i64) -> Result<(bool, bool)> {
        const PAGES: usize = 4;
        let model = |i: &Value| matches!(i["type"].as_str(), Some("reasoning" | "assistant_message" | "proposed_plan"));
        let tool = |i: &Value| !model(i) && !matches!(i["type"].as_str(), Some("subagent" | "user_message" | "notification" | "checkpoint" | "handoff"));
        let ended = |i: &Value| i["updatedAt"].as_str().and_then(at);
        let mut args = json!({"threadId":thread,"view":"activity","limit":ACTIVITY_PAGE,"maxCharsPerItem":1,"runLimit":1});
        if let Some(p) = after {
            args["afterPosition"] = json!(p);
        }
        let (mut beside, mut first, mut tool_after) = (false, true, false);
        for _ in 0..PAGES {
            let page = self.client.call("t3_thread_read", args.clone()).await?;
            let items = page["items"].as_array().cloned().unwrap_or_default();
            for i in &items {
                if first && i["type"] != "subagent" {
                    first = false;
                    beside = ended(i).is_some_and(|at| at < closed);
                }
                if i["runId"] != run {
                    continue;
                }
                if tool(i) && ended(i).is_some_and(|at| at > closed) {
                    tool_after = true;
                } else if tool_after && model(i) {
                    return Ok((beside, true));
                }
            }
            match (page["nextPosition"].as_i64(), page["hasMore"] == true && !items.is_empty()) {
                (Some(next), true) if args["afterPosition"].as_i64().is_none_or(|p| next > p) => args["afterPosition"] = json!(next),
                _ => break,
            }
        }
        Ok((beside, false))
    }
    async fn monitoring(&self,unavailable:bool,permit:&DeliveryPermit)->Result<()> {
        permit.check()?;
        let (old,episode)=self.actions.monitor_state()?;
        let episode=episode+ i64::from(unavailable && !old);
        if unavailable && !old {
            self.actions.set_monitor(unavailable,episode)?;
        }
        if unavailable || old {
            let key=format!("monitor:{episode}:{}",if unavailable{"unavailable"}else{"recovered"});
            let text=if unavailable{"Worker monitoring unavailable; owned work is retained. Retrying automatically."}else{"Worker monitoring recovered."};
            self.actions.notice(&key,text,if unavailable{"warning"}else{"success"},None,permit)?;
            // Island only: the agent is not woken about worker monitoring (E7.6).
            let _=self.db.lock().unwrap().execute("INSERT OR IGNORE INTO notices VALUES(?1,'monitor',NULL,'monitor')",[&key]);
        }
        if !unavailable && old {self.actions.set_monitor(false,episode)?;}
        Ok(())
    }
    /// One followed thread: its new activity (children), its state, its questions, and its result once done.
    async fn scan(&self, w: &Worker, permit: &DeliveryPermit) -> Result<()> {
        // Open sub-agents change in place: re-read them first, so a child that closed before the state read below
        // is never counted as open while T3 has already started the turn that reads its result.
        let subagents: Vec<String> = self.db.lock().unwrap().prepare("SELECT id FROM children WHERE thread=? AND kind='subagent'")?.query_map([&w.thread], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for item in subagents {
            let r = self.client.call("t3_thread_read", json!({"threadId":w.thread,"view":"activity","itemId":item,"limit":1,"maxCharsPerItem":200,"runLimit":1})).await?;
            match r["items"].as_array().and_then(|i| i.first()) {
                Some(i) => self.track(&w.thread, i)?,
                None => {
                    self.db.lock().unwrap().execute("DELETE FROM children WHERE thread=? AND id=?", params![w.thread, item])?;
                }
            }
        }
        let (cursor, target, trail_until, trail_run): (Option<i64>, Option<String>, Option<i64>, Option<String>) = self.db.lock().unwrap().query_row(
            "SELECT activity_cursor,target_run,trail_until,trail_run FROM workers WHERE thread=?",
            [&w.thread],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        // Recent runs: where a queued run and the turn it waited behind are told apart (`queued_behind`).
        let mut args = json!({"threadId":w.thread,"view":"activity","limit":ACTIVITY_PAGE,"maxCharsPerItem":ACTIVITY_CHARS,"runLimit":10});
        if let Some(c) = cursor {
            args["afterPosition"] = json!(c);
        }
        let mut r = Value::Null;
        let mut stalled = false;
        for _ in 0..10 {
            r = self.client.call("t3_thread_read", args.clone()).await?;
            for item in r["items"].as_array().context("T3 activity items missing")? {
                // A Monitor's task id comes after its command line, past the page's characters per item.
                // A Monitor still starting has no task id yet: read it again on the next pass (the cursor stops before it).
                // Only briefly (review 27 Oct): an item T3 leaves open (an interrupted turn, a crashed provider) must
                // never keep the rest of the thread unread. Waited for while its run is live and it changed within
                // `MONITOR_START_MS`.
                let run_live = item["runId"].as_str().and_then(|id| r["recentRuns"].as_array().into_iter().flatten().find(|x| x["runId"] == id)).and_then(|x| x["status"].as_str()).is_some_and(|s| !terminal(s));
                let young = item["updatedAt"].as_str().and_then(at).is_some_and(|t| (crate::now_ms() as i64 * 1_000_000 - t) < MONITOR_START_MS as i64 * 1_000_000);
                if run_live && young && item["type"] == "dynamic_tool" && open(item["status"].as_str().unwrap_or("")) && monitor_tool(item["text"].as_str().unwrap_or("")) && !item["text"].as_str().unwrap_or("").contains("\"taskId\"") && item["textTruncated"] != true {
                    if let Some(p) = item["position"].as_i64() {
                        self.db.lock().unwrap().execute("UPDATE workers SET activity_cursor=? WHERE thread=?", params![p - 1, w.thread])?;
                    }
                    stalled = true;
                    break;
                }
                if item["type"] == "dynamic_tool" && item["textTruncated"] == true && monitor_tool(item["text"].as_str().unwrap_or("")) {
                    let (text, _) = self.whole_text_in(&w.thread, "activity", item.clone(), 64 * 1024).await?;
                    let mut whole = item.clone();
                    whole["text"] = json!(text);
                    self.track(&w.thread, &whole)?;
                } else if item["type"] == "command_execution" && item["textTruncated"] == true && !item["text"].as_str().unwrap_or("").contains(BACKGROUND_MARK)
                    && item["updatedAt"].as_str().and_then(at).is_some_and(|t| (crate::now_ms() as i64 * 1_000_000 - t) < UNCHECKED_HOLD_MS as i64 * 1_000_000)
                {
                    // A failed read fails the scan before the cursor passes this page: it is read again next pass.
                    let whole = self.command_text(&w.thread, item).await?;
                    self.track(&w.thread, &whole)?;
                } else {
                    self.track(&w.thread, item)?;
                }
                // Items come in position order: a run's first input says what started it (T3's own rule,
                // `threadHistoryTurnStarts`: its first user message or notification; a handoff item can come before
                // it when the model changed). 't3': T3's hand-over of finished child work (`t3_handover`).
                if let (Some(run), Some(kind @ ("user_message" | "notification")), true) = (item["runId"].as_str(), item["type"].as_str(), item["visibility"].as_str().is_none_or(|v| v == "local")) {
                    let start = if kind == "notification" { start_kind(item["text"].as_str().unwrap_or("")) } else { kind };
                    self.db.lock().unwrap().execute("INSERT OR IGNORE INTO run_starts VALUES(?,?,?)", params![w.thread, run, start])?;
                }
            }
            if stalled {
                break;
            }
            let empty = r["items"].as_array().is_none_or(|i| i.is_empty());
            if let (Some(next), false) = (r["nextPosition"].as_i64(), empty) {
                if let Some(old) = args["afterPosition"].as_i64() {
                    ensure!(next > old, "T3 activity cursor made no progress");
                }
                self.db.lock().unwrap().execute("UPDATE workers SET activity_cursor=? WHERE thread=?", params![next, w.thread])?;
                args["afterPosition"] = json!(next);
            }
            if r["hasMore"] != true || empty {
                break;
            }
        }
        let t = &r["thread"];
        let run = t["activeRunId"].as_str().or_else(|| t["latestRunId"].as_str()).map(str::to_owned);
        let state = t["status"].as_str().context("T3 task state missing")?.to_owned();
        let kind = self.kind(&w.thread)?;
        let runs = r["recentRuns"].as_array().cloned().unwrap_or_default();
        // A watched thread after its result (`TRAIL_MS`): runs T3 started by itself continue the answer; one started by a
        // message is someone else's, and ends the following (any continuation that ended before it is reported first).
        let trailing = kind == "watch" && target.is_none() && trail_until.is_some();
        if trailing && self.trail_decides(w, &runs, run.as_deref(), trail_run.as_deref(), permit).await? {
            return Ok(());
        }
        // A task's earlier turn that ended with the next run already requested: a message queued behind it (its result
        // is the task's) or T3's own turn that reads finished sub-agents (it is not). Until that run shows what started
        // it, the earlier turn stays the one followed (`started_by_message`).
        let prev = w.run.as_deref().filter(|p| run.as_deref() != Some(*p) && kind == "task");
        let ended = |id: &str| runs.iter().find(|x| x["runId"] == id).and_then(|x| x["completedAt"].as_str());
        let behind = match prev.and_then(|p| queued_behind(&runs, p)) {
            Some((status, Some(next))) => match self.started_by_message(&w.thread, &next, prev.and_then(ended))? {
                None => return Ok(()),
                Some(by_message) => Some((status, by_message)),
            },
            Some((status, None)) => Some((status, false)),
            None => None,
        };
        // The turns since the one followed last pass that ended without a notice (review 21 Oct: "Two of three back:
        // A…, B…; waiting on C", then T3's turn "All done"): their replies ride in the next notice. A watched thread
        // only once its target run is over (the turns before it are the user's own). One delivered below as answered
        // before a queued message drops out of the list with its own notice.
        if let Some(p) = w.run.as_deref().filter(|p| run.as_deref() != Some(*p) && (kind == "task" || target.is_none())) {
            self.withhold(&w.thread, &runs, p, run.as_deref())?;
        }
        // A watched thread is followed for the run Heimdall messaged; a later run there is the user's own again (unless
        // it is the turn T3 starts to read that run's sub-agents, which the hold below covers).
        self.db.lock().unwrap().execute("UPDATE workers SET run=?,status=? WHERE thread=?", params![run, state, w.thread])?;
        // The project, for the sweep of sub-agent questions (`subagent_requests`).
        if let Some(project) = t["projectId"].as_str().filter(|p| !p.is_empty()) {
            self.db.lock().unwrap().execute("UPDATE workers SET project=?1 WHERE thread=?2 AND project!=?1", params![project, w.thread])?;
        }
        let current = Worker { thread: w.thread.clone(), run: run.clone(), status: state.clone() };
        let who = self.who(&w.thread, t["title"].as_str(), &kind)?;
        // Reading the thread's requests never stands between a finished run and its notice (bug notes
        // 2026-11-02T16-05-08, 16-19-09: the listing failed on a completed task that T3 still counted a hook-answered
        // approval on, and the scan stopped before the notice). A live run's failed read is retried next pass.
        if t["pendingRequestCount"].as_u64().unwrap_or(0) > 0 {
            if let Err(e) = self.pending_requests(&t, &current, &who, &state, permit).await {
                if !terminal(&state) {
                    return Err(e);
                }
                eprintln!("t3: requests of {} not read ({e:#}); its finished run is reported anyway", w.thread);
            }
        }
        if terminal(&state) || behind.is_some() {
            // A finished turn: which of its background commands really still run (bug note 01-28-46).
            self.settle_background(&w.thread).await?;
        }
        let mut open = self.open_children(&w.thread)?;
        // A task's turn that ended while a message queued behind it was already waiting: T3 starts the queued turn at
        // once, so the thread never shows this turn finished. Its result is still the task's (the prompt sends a later
        // request to a task with mode queue). A turn T3 queued to read finished sub-agents is not a message: the
        // earlier turn was progress, and the turn that reads them brings the result (bug note 19-19-06).
        if let (Some(prev), Some((status, true))) = (prev, behind.as_ref()) {
            let done = Worker { thread: w.thread.clone(), run: Some(prev.to_owned()), status: status.clone() };
            let still = crate::text::still_with(&open.agents, &open.stale, &open.unchecked);
            self.terminal_notice(&done, status, still.as_deref(), true, permit).await?;
        }
        if let Some(target) = target {
            // The run Heimdall's message went into (a queued send: behind the turn that was running then).
            let mine = runs.iter().find(|x| x["runId"] == target.as_str());
            match mine.and_then(|m| m["status"].as_str()) {
                // Not over yet: the thread's other turns ending is not its reply.
                Some(s) if !terminal(s) => return Ok(()),
                Some(s) if run.as_deref() != Some(target.as_str()) => {
                    // Over, and a later run exists. One queued before this run ended by a message is someone else's:
                    // the reply is this run's own (unless its sub-agents still run: then the later turn may read
                    // them). One T3 queued to read finished sub-agents is the reply's continuation, followed below.
                    let by_message = match queued_behind(&runs, &target) {
                        Some((_, Some(next))) => match self.started_by_message(&w.thread, &next, ended(&target))? {
                            None => return Ok(()),
                            Some(m) => m,
                        },
                        _ => false,
                    };
                    if by_message && open.agents.is_empty() && open.commands.is_empty() {
                        self.db.lock().unwrap().execute("UPDATE workers SET target_run=NULL WHERE thread=?", [&w.thread])?;
                        let mine = Worker { thread: w.thread.clone(), run: Some(target.clone()), status: s.to_owned() };
                        let still = crate::text::still_with(&[], &open.stale, &open.unchecked);
                        return self.terminal_notice(&mine, s, still.as_deref(), false, permit).await;
                    }
                    // The latest run answers instead: this one's reply, and those of the turns after it, ride in that
                    // notice.
                    self.withhold(&w.thread, &runs, &target, run.as_deref())?;
                    self.db.lock().unwrap().execute("UPDATE workers SET target_run=NULL WHERE thread=?", [&w.thread])?;
                }
                // Over and the latest run, or no longer among the recent runs: the latest run decides, as for a task
                // (with this one's reply before its own when it is a later one).
                _ => {
                    if run.as_deref() != Some(target.as_str()) {
                        self.withhold(&w.thread, &runs, &target, run.as_deref())?;
                    }
                    self.db.lock().unwrap().execute("UPDATE workers SET target_run=NULL WHERE thread=?", [&w.thread])?;
                }
            }
        }
        if trailing && terminal(&state) && open.agents.is_empty() && open.commands.is_empty() && trail_until.is_some_and(|t| crate::now_ms() as i64 > t) {
            // Quiet since the last notice for `TRAIL_MS`, its run reported: the thread is the user's again.
            let key = format!("terminal:{}:{}:{state}", w.thread, run.as_deref().unwrap_or(""));
            let noticed = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM notices WHERE id=?)", [&key], |r| r.get::<_, bool>(0))?;
            let held = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM held WHERE thread=?)", [&w.thread], |r| r.get::<_, bool>(0))?;
            if noticed && !held {
                self.unwatch(&w.thread, &kind)?;
                return Ok(());
            }
        }
        if !terminal(&state) {
            // Working again (a new turn): any held earlier turn is superseded.
            self.db.lock().unwrap().execute("DELETE FROM held WHERE thread=?", [&w.thread])?;
            return Ok(());
        }
        let Some(run) = run else { return Ok(()) };
        let held: Option<(String, String, i64, Option<i64>)> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT run,state,since_ms,clear_ms FROM held WHERE thread=?", [&w.thread], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .optional()?
            .filter(|(r, _, _, _)| *r == run);
        let now = crate::now_ms();
        let (grace, _, agent_hold) = *self.hold.lock().unwrap();
        // Before a finished turn counts as the result: a sub-agent re-opened in place is open work again, and one
        // that closed during this turn means T3 is about to start the turn that reads it.
        let mut closed_during = false;
        if state == "completed" && open.agents.is_empty() {
            let started = runs.iter().find(|x| x["runId"] == run.as_str()).and_then(|x| x["startedAt"].as_str()).and_then(at);
            let (reopened, during) = self.recheck_subagents(&w.thread, &run, started).await?;
            if reopened {
                open = self.open_children(&w.thread)?;
            }
            closed_during = during;
        }
        let waiting = !open.agents.is_empty() || !open.commands.is_empty();
        if state == "completed" {
            if let Some((_, _, _, clear)) = held.as_ref().filter(|(_, s, _, _)| s == "released") {
                // Delivered at the hold limit while sub-agents still ran: the turn that reads them is still followed
                // (a watched thread stays watched); once they close and no such turn comes, following ends quietly.
                if waiting {
                    self.db.lock().unwrap().execute("UPDATE held SET clear_ms=NULL WHERE thread=?", [&w.thread])?;
                    return Ok(());
                }
                match clear {
                    None => {
                        self.db.lock().unwrap().execute("UPDATE held SET clear_ms=? WHERE thread=?", params![now as i64, w.thread])?;
                    }
                    Some(c) if now.saturating_sub(*c as u64) < grace => {}
                    Some(_) => {
                        self.db.lock().unwrap().execute("DELETE FROM held WHERE thread=?", [&w.thread])?;
                        self.unwatch(&w.thread, &kind)?;
                    }
                }
                return Ok(());
            }
            if waiting {
                // T3's waiting_for_children: a finished turn while its own work still runs is not the result.
                let Some((_, _, since, _)) = held else {
                    self.db.lock().unwrap().execute("INSERT OR REPLACE INTO held VALUES(?,?,?,?,NULL)", params![w.thread, run, state, now as i64])?;
                    return Ok(());
                };
                self.db.lock().unwrap().execute("UPDATE held SET clear_ms=NULL WHERE thread=?", [&w.thread])?;
                // Background commands have their own limit (`open_children`); sub-agents are held this long at most.
                if !open.commands.is_empty() || now.saturating_sub(since as u64) < agent_hold {
                    return Ok(());
                }
                // The hold limit: the result goes out now, with the sub-agents it still waits on.
                eprintln!(
                    "t3: hold limit reached for {} run {run} after {} min; delivering while sub-agents still run: {}",
                    w.thread,
                    now.saturating_sub(since as u64) / 60_000,
                    open.agents.join("; ")
                );
                self.db.lock().unwrap().execute("UPDATE held SET state='released',clear_ms=NULL WHERE thread=?", [&w.thread])?;
                let still = crate::text::still_with(&open.agents, &open.stale, &open.unchecked);
                return self.terminal_notice(&current, &state, still.as_deref(), true, permit).await;
            }
            if let Some((_, _, _, clear)) = held {
                // Children closed after this turn ended: T3 normally starts a turn to read them; wait for it a moment.
                match clear {
                    None => {
                        self.db.lock().unwrap().execute("UPDATE held SET clear_ms=? WHERE thread=?", params![now as i64, w.thread])?;
                        return Ok(());
                    }
                    Some(c) if now.saturating_sub(c as u64) < grace => return Ok(()),
                    Some(_) => {}
                }
            } else if closed_during {
                // A background sub-agent closed while this turn ran (20 Oct 19:18: "Two of the three research
                // workers are back. Still waiting on …", the third had closed 3 s earlier): the same short wait for
                // the turn T3 starts to hand it over, which then supersedes this one.
                self.db.lock().unwrap().execute("INSERT OR REPLACE INTO held VALUES(?,?,?,?,?)", params![w.thread, run, state, now as i64, now as i64])?;
                return Ok(());
            }
        }
        self.db.lock().unwrap().execute("DELETE FROM held WHERE thread=?", [&w.thread])?;
        // Bug note 22-19-38 (task mcp:6b581c50, 22 Oct 22:07-22:35): a worker ended a turn after each benchmark shard
        // ("Shard 3 of 8 is running"), and each woke Heimdall with the same "still has background work running" list.
        // Heimdall has already been told about every background command this turn still waits on, so the turn is
        // progress: withheld, its reply rides in the next notice (the background work closing, a turn ending with
        // nothing open, a new command past its hold). Only the first such notice goes out (bug note 16-38-55), and
        // never more than `SUBAGENT_HOLD_MS` after it. Never withheld: a failed or stopped turn, one with a question,
        // one a message started (run 9 of that task, "GO INSTALL", is Heimdall's request; its answer is news), and one
        // T3 started to hand over finished sub-agents.
        let asks = crate::threads::live_requests(&t) > 0;
        let recent = |n: Option<u64>| n.is_some_and(|n| now.saturating_sub(n) < agent_hold);
        if state == "completed" && !asks && open.agents.is_empty() && (recent(open.repeat) || recent(open.repeat_all)) {
            let start: Option<String> = self.db.lock().unwrap().query_row("SELECT kind FROM run_starts WHERE thread=? AND run=?", params![w.thread, run], |r| r.get(0)).optional()?;
            // Only a background command's own notification (a monitor's stop included; monitors themselves do not
            // count): T3's hand-over of finished sub-agents ("t3") is news (review).
            if start.as_deref() == Some("notification") && recent(open.repeat) {
                return Ok(());
            }
            // A Monitor's event (bug notes 2026-10-24T20-06-04, 2026-10-25T21-22-04: "Labelling is at 135 of 318…
            // still waiting", "Shard 4 of 16 is done"), unless a sub-agent closed since that notice: then T3's
            // "Background activity updated" is handing its result over.
            if start.as_deref() == Some("activity") && open.monitors && recent(open.repeat_all) {
                let since = open.repeat_all.unwrap_or(0) as i64;
                let handed = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM closes WHERE thread=? AND ms>=?)", params![w.thread, since], |r| r.get::<_, bool>(0))?;
                if !handed {
                    return Ok(());
                }
            }
        }
        let still = crate::text::still_with(&[], &open.stale, &open.unchecked);
        self.terminal_notice(&current, &state, still.as_deref(), false, permit).await
    }
    /// A followed thread's pending questions (each once) and approval wait. An approval holds a running turn: once the
    /// run has ended T3's leftover count is stale, never the user's to give (bug notes 2026-10-28T02-20-08,
    /// 2026-11-02T14-56-51: T3 keeps the approval Heimdall's hook answered "pending" until the provider session
    /// closes). Questions T3 still lists are reported whatever the run's state.
    async fn pending_requests(&self, t: &Value, current: &Worker, who: &str, state: &str, permit: &DeliveryPermit) -> Result<()> {
        let thread = current.thread.as_str();
        let ids = crate::question_ids(&self.client, thread).await?;
        for id in &ids {
            let seen = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM questions WHERE thread=? AND id=?)", params![thread, id], |r| r.get::<_, bool>(0))?;
            if !seen {
                let q = crate::read_question(&self.client, thread, id).await?;
                self.emit(&format!("question:{thread}:{id}"), current, "question", &format!("{who} asks [request {id}]:\n{}", crate::text::question(&q)), permit).await?;
                self.db.lock().unwrap().execute("INSERT OR IGNORE INTO questions VALUES(?,?)", params![thread, id])?;
            }
        }
        if t["pendingRequestCount"].as_u64().unwrap_or(0) > ids.len() as u64 && !terminal(state) && crate::waits_on_approval(&self.client, thread, t).await? {
            self.emit(
                &format!("approval:{thread}:{}", current.run.as_deref().unwrap_or("idle")),
                current,
                "approval",
                &format!("{who} waits for an approval in T3; only User can give it there."),
                permit,
            )
            .await?;
        }
        Ok(())
    }
    /// A watched thread after its result: the runs after `trail_run` (the last one reported). One whose first input
    /// is a message (the user's own turn), or that ended without any first input seen, ends the following; before that,
    /// the last continuation run that ended unreported is reported (with the ones before it). A run not started yet
    /// (queued, or no item yet) is decided on a later pass. Returns whether the following ended.
    async fn trail_decides(&self, w: &Worker, runs: &[Value], current: Option<&str>, trail_run: Option<&str>, permit: &DeliveryPermit) -> Result<bool> {
        let ordinal = |id: &str| runs.iter().find(|x| x["runId"] == id).and_then(|x| x["ordinal"].as_i64());
        let from = trail_run.and_then(ordinal);
        let mut later: Vec<&Value> = runs
            .iter()
            .filter(|x| match from {
                Some(f) => x["ordinal"].as_i64().is_some_and(|o| o > f),
                // The last reported run is no longer among the recent ones: only the current run is judged.
                None => x["runId"].as_str() == current && x["runId"].as_str() != trail_run,
            })
            .collect();
        later.sort_by_key(|x| x["ordinal"].as_i64());
        let mut continued: Option<&Value> = None;
        for x in later {
            let id = x["runId"].as_str().unwrap_or("");
            let status = x["status"].as_str().unwrap_or("");
            let start: Option<String> = self.db.lock().unwrap().query_row("SELECT kind FROM run_starts WHERE thread=? AND run=?", params![w.thread, id], |r| r.get(0)).optional()?;
            match start.as_deref() {
                Some("user_message") => {}
                None if terminal(status) => {}
                Some(_) => {
                    if terminal(status) {
                        continued = Some(x);
                    }
                    continue;
                }
                None => continue,
            }
            eprintln!("t3: {} run {id} was not started by T3 itself; following the thread ends", w.thread);
            if let Some(c) = continued {
                let (run, state) = (c["runId"].as_str().unwrap_or("").to_owned(), c["status"].as_str().unwrap_or("completed").to_owned());
                if let Some(t) = trail_run {
                    self.withhold(&w.thread, runs, t, Some(&run))?;
                }
                let done = Worker { thread: w.thread.clone(), run: Some(run), status: state.clone() };
                self.terminal_notice(&done, &state, None, true, permit).await?;
            }
            self.unwatch(&w.thread, "watch")?;
            return Ok(true);
        }
        Ok(false)
    }
    /// "Task 'name' (id)" or "Thread 'name' (id)" for a watched thread: who a notice is about.
    fn who(&self, thread: &str, live: Option<&str>, kind: &str) -> Result<String> {
        Ok(match self.title(thread, live)?.map(|t| crate::text::title(&t)).filter(|t| !t.is_empty()) {
            Some(name) if kind == "watch" => format!("Thread '{name}' ({thread})"),
            Some(name) => format!("Task '{name}' ({thread})"),
            None if kind == "watch" => format!("Thread {thread}"),
            None => format!("Task {thread}"),
        })
    }
    fn reconcile_monitor_failures(&self) -> Result<()> {
        let db=self.db.lock().unwrap();
        let keys=db.prepare("SELECT thread,run FROM workers WHERE archived=0 AND status IN ('preparing','queued','starting','running','waiting') AND run IS NOT NULL UNION SELECT thread,run FROM pending_finals")?.query_map([],|r|Ok(format!("{}:{}",r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
        self.actions.reconcile_monitor_failures(&keys)
    }
    /// Whether a followed thread needs this pass's full scan: it is working, holds a turn back, waits on children,
    /// asks something, or its latest turn has no notice yet. A settled one costs one state read per pass.
    async fn needs_scan(&self, w: &Worker) -> Result<bool> {
        if w.run.is_none() || !terminal(&w.status) {
            return Ok(true);
        }
        let pending: bool = self.db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM held WHERE thread=?1) OR EXISTS(SELECT 1 FROM children WHERE thread=?1) OR EXISTS(SELECT 1 FROM workers WHERE thread=?1 AND (target_run IS NOT NULL OR trail_until<?2))",
            params![w.thread, crate::now_ms() as i64],
            |r| r.get(0),
        )?;
        if pending {
            return Ok(true);
        }
        let r = self.client.call("t3_thread_read", json!({"threadId":w.thread,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?;
        let t = &r["thread"];
        let run = t["activeRunId"].as_str().or_else(|| t["latestRunId"].as_str());
        if run != w.run.as_deref() || t["status"].as_str() != Some(w.status.as_str()) || crate::threads::live_requests(t) > 0 {
            return Ok(true);
        }
        let key = format!("terminal:{}:{}:{}", w.thread, w.run.as_deref().unwrap_or(""), w.status);
        Ok(!self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM notices WHERE id=?)", [key], |r| r.get::<_, bool>(0))?)
    }
    /// Every followed thread once: a full scan where `needs_scan` says so. A failure is returned after all threads
    /// had their turn.
    pub async fn scan_all(&self, permit: &DeliveryPermit) -> Result<()> {
        self.retire_carried()?;
        let mut failed = None;
        for w in self.all()? {
            let r = async {
                if permit.run(self.needs_scan(&w)).await? {
                    permit.run(self.scan(&w, permit)).await
                } else {
                    Ok(())
                }
            }
            .await;
            if let Err(e) = r {
                // A rejected credential is the one the caller must see (it asks the user to connect again).
                let auth = |e: &anyhow::Error| e.to_string().starts_with("T3_AUTH_REQUIRED");
                if failed.as_ref().is_none_or(|f| !auth(f)) {
                    failed = Some(e);
                }
            }
        }
        if let Err(e) = permit.run(self.subagent_requests(permit)).await {
            if failed.is_none() {
                failed = Some(e);
            }
        }
        // After the scans: a task that recovered by itself since the last pass has ended its episode by now.
        if let Err(e) = self.auth_retries(permit).await {
            failed.get_or_insert(e);
        }
        // Tasks whose job island the user closed, finished and reported by now: archived.
        if let Err(e) = permit.run(self.archive_cleared()).await {
            failed.get_or_insert(e);
        }
        failed.map_or(Ok(()), Err)
    }
    /// Questions and approvals of sub-agent threads (T3 `delegate_task` children, `relationshipToParent:
    /// "subagent"`), reported as their followed ancestor's: T3 counts a child's request only on the child, the
    /// observer leaves sub-agent threads to their parent, and a result held for that sub-agent would otherwise wait
    /// on a question nobody hears (review 20 Oct). One list of waiting threads per project with work in flight.
    async fn subagent_requests(&self, permit: &DeliveryPermit) -> Result<()> {
        let (projects, followed): (Vec<String>, std::collections::HashSet<String>) = {
            let db = self.db.lock().unwrap();
            let projects = db
                .prepare(
                    "SELECT DISTINCT project FROM workers WHERE archived=0 AND project!='' AND (status IN ('preparing','queued','starting','running','waiting')
                     OR EXISTS(SELECT 1 FROM held WHERE held.thread=workers.thread) OR EXISTS(SELECT 1 FROM children WHERE children.thread=workers.thread))",
                )?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let followed = db.prepare("SELECT thread FROM workers WHERE archived=0")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            (projects, followed)
        };
        for project in projects {
            permit.check()?;
            // Live threads, so a request T3 counts on a running child is seen as the observer sees one (bug note
            // 2026-11-01T00-15-42, `threads::asks`).
            let rows = self.page_threads(&project, &crate::threads::ACTIVE, 100, 5, true).await?;
            let parent_of = |id: &str| rows.iter().find(|r| r["threadId"] == id).and_then(|r| r["parentThreadId"].as_str().map(str::to_owned));
            for child in rows.iter().filter(|r| r["relationshipToParent"] == "subagent" && crate::threads::asks(r)) {
                let Some(id) = child["threadId"].as_str() else { continue };
                // The nearest followed ancestor; a sub-agent's own sub-agent reports to the same task.
                let mut parent = child["parentThreadId"].as_str().map(str::to_owned);
                let mut root = None;
                for _ in 0..5 {
                    let Some(p) = parent.take() else { break };
                    if followed.contains(&p) {
                        root = Some(p);
                        break;
                    }
                    parent = match parent_of(&p) {
                        Some(up) => Some(up),
                        None => self.client.call("t3_thread_read", json!({"threadId":p,"limit":1,"maxCharsPerItem":1,"runLimit":1})).await?["thread"]["parentThreadId"]
                            .as_str()
                            .map(str::to_owned),
                    };
                }
                let Some(root) = root else { continue };
                self.subagent_request(&root, id, child, permit).await?;
            }
        }
        Ok(())
    }
    /// One waiting sub-agent thread's questions (each once) or its approval wait, as `root`'s notice. The thread id
    /// in the line is the one an answer (`t3_pending_request` respond) needs.
    async fn subagent_request(&self, root: &str, child: &str, row: &Value, permit: &DeliveryPermit) -> Result<()> {
        let w = self.db.lock().unwrap().query_row("SELECT thread,run,status FROM workers WHERE thread=?", [root], |r| {
            Ok(Worker { thread: r.get(0)?, run: r.get(1)?, status: r.get(2)? })
        })?;
        let who = self.who(root, None, &self.kind(root)?)?;
        let name = crate::text::title(row["title"].as_str().unwrap_or(""));
        let name = if name.trim().is_empty() { "sub-agent".to_owned() } else { name.replace(['\'', '\n'], " ") };
        let ids = crate::question_ids(&self.client, child).await?;
        for id in &ids {
            let seen = self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM questions WHERE thread=? AND id=?)", params![child, id], |r| r.get::<_, bool>(0))?;
            if seen {
                continue;
            }
            let q = crate::read_question(&self.client, child, id).await?;
            let text = format!("{who} asks through its sub-agent '{name}' [thread {child}, request {id}]:\n{}", crate::text::question(&q));
            self.emit(&format!("question:{child}:{id}"), &w, "question", &text, permit).await?;
            self.db.lock().unwrap().execute("INSERT OR IGNORE INTO questions VALUES(?,?)", params![child, id])?;
        }
        // A card Heimdall's permission hook already answered (the hook answers delegate children of Heimdall's threads
        // too) is no wait (`crate::waits_on_approval`).
        if ids.is_empty() && crate::waits_on_approval(&self.client, child, row).await? {
            let run = row["latestRunId"].as_str().unwrap_or("idle");
            let text = format!("{who}: its sub-agent '{name}' waits for an approval in T3; only User can give it there [thread {child}].");
            self.emit(&format!("approval:{child}:{run}"), &w, "approval", &text, permit).await?;
        }
        Ok(())
    }
    /// Bug note 01-28-46 and ("none lost or duplicated"): the safety net behind the watcher, for an
    /// app tick (the watcher runs parts 1 and 3 itself on every pass, so also at startup). Every notice keeps its
    /// deterministic id, so nothing here ever wakes the agent twice for one run.
    /// 1. The outbox drains: a notice left waiting at a quit, or one whose delivery a quit cut short (the agent's
    ///    deliveries are deduplicated by id), goes out, even while T3 is unreachable.
    /// 2. A followed thread whose latest run has finished with no notice, not held back and with no open children:
    ///    the scan that should have reported it runs now (it reads T3).
    /// 3. A followed thread whose latest run's notice never reached the agent (its outbox row dropped for a newer
    ///    delivery epoch, or gone): the result is delivered once more, as `{id}:again`.
    /// Returns how many threads it acted on (2 and 3).
    pub async fn heartbeat(&self, permit: &DeliveryPermit) -> Result<usize> {
        let _pass = self.pass.lock().await;
        self.audit(true, permit).await
    }
    async fn audit(&self, rescan: bool, permit: &DeliveryPermit) -> Result<usize> {
        permit.check()?;
        // An undeliverable row or an acknowledgement timeout must not end the pass before its checks (and, in the
        // watcher, before the per-run waits are armed): the rows stay in the outbox for the next drain (night 3
        // review).
        match self.actions.drain(permit) {
            Ok(()) => self.audit_drain_failing.store(false, std::sync::atomic::Ordering::Relaxed),
            Err(e) => {
                permit.check()?;
                if !self.audit_drain_failing.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    eprintln!("t3: outbox not drained before the audit (its rows wait for the next drain): {e:#}");
                }
            }
        }
        self.retire_carried()?;
        let rows: Vec<(Worker, bool)> = self
            .db
            .lock()
            .unwrap()
            .prepare(
                "SELECT thread,run,status,EXISTS(SELECT 1 FROM held WHERE held.thread=workers.thread) OR EXISTS(SELECT 1 FROM children WHERE children.thread=workers.thread)
                 FROM workers WHERE archived=0 AND run IS NOT NULL ORDER BY rowid",
            )?
            .query_map([], |r| Ok((Worker { thread: r.get(0)?, run: r.get(1)?, status: r.get(2)? }, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let (mut acted, mut failed) = (0, None);
        for (w, _) in rows.into_iter().filter(|(w, busy)| terminal(&w.status) && !busy) {
            let key = format!("terminal:{}:{}:{}", w.thread, w.run.as_deref().unwrap_or(""), w.status);
            let noticed: Option<String> = self.db.lock().unwrap().query_row("SELECT kind FROM notices WHERE id=?", [&key], |r| r.get(0)).optional()?;
            let r = match noticed.as_deref() {
                None if rescan => {
                    acted += 1;
                    permit.run(self.scan(&w, permit)).await
                }
                Some("terminal") if matches!(self.actions.outbox_state(&key)?.map(|(d, _)| d), None | Some(-1)) => {
                    let again = format!("{key}:again");
                    if self.db.lock().unwrap().query_row("SELECT EXISTS(SELECT 1 FROM notices WHERE id=?)", [&again], |r| r.get::<_, bool>(0))? {
                        continue;
                    }
                    eprintln!("t3: the notice {key} never reached the agent; delivering it again");
                    acted += 1;
                    permit.run(self.terminal_notice_as(&w, &w.status, None, true, Some(again), permit)).await
                }
                _ => continue,
            };
            if let Err(e) = r {
                failed.get_or_insert(e);
            }
        }
        failed.map_or(Ok(acted), Err)
    }
    /// The withheld turns of result notices whose delivery failed at first (`carried`): retired once the notice or
    /// the heartbeat's `:again` resend of it reached the agent.
    fn retire_carried(&self) -> Result<()> {
        let keys: Vec<String> = self.db.lock().unwrap().prepare("SELECT DISTINCT notice FROM carried")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for key in keys {
            let delivered = |id: &str| -> Result<bool> { Ok(self.actions.outbox_state(id)?.is_some_and(|(d, _)| d == 1)) };
            if delivered(&key)? || delivered(&format!("{key}:again"))? {
                let db = self.db.lock().unwrap();
                db.execute("DELETE FROM withheld WHERE EXISTS(SELECT 1 FROM carried c WHERE c.notice=?1 AND c.thread=withheld.thread AND c.run=withheld.run)", [&key])?;
                db.execute("DELETE FROM carried WHERE notice=?1", [&key])?;
            }
        }
        Ok(())
    }
    /// After a restart, one short notice listing the T3 work the agent still follows, one line
    /// each with its state, so it can re-check them. None when nothing is followed.
    pub fn restart_notice(&self, reachable: bool) -> Result<Option<String>> {
        let rows: Vec<(String, Option<String>, String, String)> = self
            .db
            .lock()
            .unwrap()
            .prepare("SELECT thread,title,status,kind FROM workers WHERE archived=0 ORDER BY rowid")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut lines = vec![if reachable {
            "Heimdall restarted. T3 work you still follow (its results still arrive as notices):".to_owned()
        } else {
            "Heimdall restarted; T3 is not reachable yet. T3 work you still follow, as last seen:".to_owned()
        }];
        for (thread, title, status, kind) in rows {
            let open = self.open_children(&thread)?;
            let state = crate::text::task_state(&status, open.agents.len(), open.commands.len());
            let what = if kind == "watch" { "thread" } else { "task" };
            lines.push(format!("- {what} {}", crate::text::task_line(&thread, title.as_deref(), &state, "")));
        }
        Ok(Some(lines.join("\n")))
    }
    /// The restart notice, once per app process and store; a later start of the agent in the same process (a model
    /// switch) is not a restart. Once it is in the outbox it is announced: a drain that times out (Pi still starting)
    /// leaves it to the next drain under the same id, never a second notice with fresh text; one an Agent Off/On
    /// dropped before it reached the agent is re-armed under that id (night 3 review).
    async fn announce_restart(&self, reachable: bool, permit: &DeliveryPermit) -> Result<()> {
        let announced = ANNOUNCED.lock().unwrap().iter().find(|(p, _)| *p == self.path).map(|(_, id)| id.clone());
        match announced {
            Some(None) => return Ok(()),
            Some(Some(id)) => {
                let dropped = self.actions.outbox_state(&id)?.is_some_and(|(d, epoch)| d == -1 || (d == 0 && epoch != permit.generation() as i64));
                if !dropped {
                    return Ok(());
                }
                let Some(text) = self.restart_notice(reachable)? else { return Ok(()) };
                self.actions.outbox_rearm(&id, &text, permit)?;
            }
            None => {
                let text = self.restart_notice(reachable)?;
                let id = text.as_ref().map(|_| format!("t3-restarted:{}", crate::now_ms()));
                if let (Some(id), Some(text)) = (&id, &text) {
                    self.actions.enqueue(id, text, permit)?;
                }
                ANNOUNCED.lock().unwrap().push((self.path.clone(), id.clone()));
                if id.is_none() {
                    return Ok(());
                }
            }
        }
        if let Err(e) = self.actions.drain(permit) {
            eprintln!("t3: restart notice waits in the outbox: {e:#}");
        }
        Ok(())
    }
    /// One event-backed wait per active run and one 30-second control-plane pass.
    /// Gate cancellation drops wait futures; terminal/idle runs never re-arm.
    pub async fn watch(
        self: Arc<Self>,
        gate: Arc<DeliveryGate>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        if *stop.borrow() {
            return;
        }
        let inner = self.watch_inner(gate, stop.clone());
        tokio::select! {
            biased;
            _=async{loop{if stop.changed().await.is_err()||*stop.borrow(){break;}}}=>{},
            _=inner=>{},
        }
    }
    async fn watch_inner(
        self: Arc<Self>,
        gate: Arc<DeliveryGate>,
        mut stop: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tasks: tokio::task::JoinSet<(String, Worker, Result<Value>)> =
            tokio::task::JoinSet::new();
        let mut waiting = std::collections::HashSet::new();
        // Event-backed waits on other threads' runs (observer.rs): a completion triggers a pass at once.
        let mut global: tokio::task::JoinSet<(String, Result<Value>)> = tokio::task::JoinSet::new();
        let mut global_waiting = std::collections::HashSet::new();
        // A run whose wait already returned is never waited on again: no hot loop if T3's list and wait disagree.
        let mut global_done = std::collections::HashSet::new();
        // The end of a held result's grace that already started a pass: a failing pass retries on the tick, never in a loop.
        let mut hold_fired: Option<u64> = None;
        loop {
            // A held result is decided when its grace ends, not at the next 30-second pass (bug note 22 Oct 15:35).
            let hold_due = self.hold_due(hold_fired);
            tokio::select! {
                biased;
                _=stop.changed()=>{if *stop.borrow(){break;}},
                joined=tasks.join_next(),if !tasks.is_empty()=>{
                    if let Some(Ok((key,w,r)))=joined {
                        waiting.remove(&key);
                        if r.as_ref().err().is_some_and(|e|e.downcast_ref::<agent::delivery::DeliveryBlocked>().is_some()){continue;}
                        let timed_out=r.as_ref().is_ok_and(|v|v["timedOut"]==true);
                        if let Ok(permit)=gate.permit() {
                            let _pass=self.pass.lock().await;
                            let result=async {
                                let result=r?;
                                if let Some(state)=result["status"].as_str().filter(|s|terminal(s)) {
                                    permit.with(||{self.db.lock().unwrap().execute("UPDATE workers SET status=? WHERE thread=? AND run=?",params![state,w.thread,w.run])?;Ok(())})?;
                                }
                                // The scan decides: a result, a held turn (children still working), or a new turn.
                                permit.run(self.scan(&w,&permit)).await
                            }.await;
                            self.pass_done();
                            if permit.check().is_ok(){
                                let _=self.actions.monitor_failure(&key,result.is_err());
                                if result.is_err(){let _=self.monitoring(true,&permit).await;}
                                // A wait that timed out on a run still working waits again at once: a run ending in the
                                // gap before the next pass is reported when it ends, not up to 30 s later.
                                if timed_out && result.is_ok() {
                                    for w in self.active().unwrap_or_default().into_iter().filter(|x|x.thread==w.thread) {self.arm(&mut tasks,&mut waiting,w,&permit);}
                                }
                            }
                        }
                    } else if let Ok(permit)=gate.permit() {
                        tasks.abort_all();waiting.clear();
                        let _=self.actions.monitor_failure("wait-supervisor",true);
                        let _=self.monitoring(true,&permit).await;
                    }

                },
                joined=global.join_next(),if !global.is_empty()=>{
                    if let Some(Ok((key,r)))=joined {
                        global_waiting.remove(&key);
                        if r.is_ok_and(|v|v["timedOut"]!=true) {if global_done.len()>10_000{global_done.clear();}global_done.insert(key);self.changed.notify_one();}
                    }
                },
                _=async{tokio::select!{_=tick.tick()=>{},_=self.changed.notified()=>{},_=hold_sleep(hold_due)=>{}}}=>{
                    if hold_due.is_some_and(|due|crate::now_ms()>=due) {hold_fired=hold_due;}
                    let Ok(permit)=gate.permit()else{tasks.abort_all();waiting.clear();global.abort_all();global_waiting.clear();continue;};
                    let pass_lock=self.pass.lock().await;
                    // Notices left waiting at a quit go out first, whatever T3's state (heartbeat part 1).
                    let _=self.actions.drain(&permit);
                    let pass=async {
                        permit.run(self.reconcile_launches(&permit)).await?;
                        permit.run(self.retry_finals(&permit)).await?;
                        // Connect (pairing automatically) before deciding whether the user must click Connect T3.
                        let reach=self.client.auto_connect().await;
                        let offline=self.reachability(&reach,&permit)?;
                        if self.connection()==super::observer::ConnectionState::SignInRequired || (!offline && self.client.expiry_notice()?) {self.connect_prompt(&permit)?;}
                        if let Err(e)=self.scan_all(&permit).await {
                            if e.to_string().starts_with("T3_AUTH_REQUIRED") {self.set_connection_sign_in();self.connect_prompt(&permit)?;}
                            return Err(e);
                        }
                        // Heartbeat part 3 (a notice that never reached the agent); the scans above were part 2.
                        permit.run(self.audit(false,&permit)).await?;
                        for w in self.active()? {self.arm(&mut tasks,&mut waiting,w,&permit);}
                        self.actions.drain(&permit)?;
                        self.retire()?;self.actions.retire()?;
                        Ok::<_,anyhow::Error>(())
                    }.await;
                    if permit.check().is_ok(){
                        // Reconcile even when final collection or scanning failed:
                        // archived/superseded wait keys no longer own retry work.
                        let reconcile=self.reconcile_monitor_failures();
                        let _=self.actions.monitor_failure("control-plane",pass.is_err() || reconcile.is_err());
                        if pass.is_ok(){let _=self.actions.monitor_failure("wait-supervisor",false);}
                        let unavailable=self.actions.monitor_failed().unwrap_or(true);
                        let _=self.monitoring(unavailable,&permit).await;
                        // The restart notice, after the first pass (states fresh from T3 when it is reachable).
                        if let Err(e)=self.announce_restart(pass.is_ok(),&permit).await {eprintln!("t3: restart notice not sent: {e:#}");}
                    }
                    drop(pass_lock);
                    // Every T3 thread, not only Heimdall's tasks. Its failures never mark worker monitoring.
                    match permit.run(self.observe(&permit)).await {
                        Ok(waits) => for (thread,run) in waits {
                            if global.len()>=super::observer::MAX_WAITS {break;}
                            let key=format!("{thread}:{run}");
                            if global_done.contains(&key) || !global_waiting.insert(key.clone()) {continue;}
                            let this=self.clone();let permit=permit.clone();
                            global.spawn(async move{let r=permit.run(this.client.call_deadline("t3_thread_wait",json!({"threadId":thread,"runId":run,"timeoutMs":55000}),Duration::from_secs(65))).await;(key,r)});
                        },
                        Err(e) => super::observer::log_failure(&e),
                    }
                    self.pass_done();

                }
            }
        }
        tasks.abort_all();
        global.abort_all();
        while tasks.join_next().await.is_some() {}
        while global.join_next().await.is_some() {}
    }
}
impl Workers {
    /// One event-backed wait on a followed run, unless one is already waiting on it.
    fn arm(
        self: &Arc<Self>,
        tasks: &mut tokio::task::JoinSet<(String, Worker, Result<Value>)>,
        waiting: &mut std::collections::HashSet<String>,
        w: Worker,
        permit: &DeliveryPermit,
    ) {
        let Some(run) = &w.run else { return };
        let key = format!("{}:{run}", w.thread);
        if !waiting.insert(key.clone()) {
            return;
        }
        let (this, permit) = (self.clone(), permit.clone());
        tasks.spawn(async move {
            let r = permit.run(this.client.call_deadline("t3_thread_wait", json!({"threadId":w.thread,"runId":w.run,"timeoutMs":55000}), Duration::from_secs(65))).await;
            (key, w, r)
        });
    }
}
/// Until `due` (wall-clock ms; at once when it has passed), or never.
async fn hold_sleep(due: Option<u64>) {
    match due {
        Some(due) => tokio::time::sleep(Duration::from_millis(due.saturating_sub(crate::now_ms()))).await,
        None => std::future::pending().await,
    }
}
/// The work a followed thread still waits on (`Workers::open_children`): sub-agent labels, background commands
/// still within `BACKGROUND_HOLD_MS`, and background commands past it.
struct Open {
    agents: Vec<String>,
    commands: Vec<String>,
    stale: Vec<String>,
    /// Background commands past the hold that nothing can check (no output file known).
    unchecked: Vec<String>,
    /// Some(ms) when every background command past the hold (`stale` and `unchecked`, at least one) was named by a
    /// delivered notice, the earliest at `ms` (bug note 22-19-38).
    repeat: Option<u64>,
    /// `repeat` over every background item past the hold, monitors included (a monitor's own event turns).
    repeat_all: Option<u64>,
    /// A Claude Monitor of the thread still runs.
    monitors: bool,
    /// `unchecked` commands started within `UNCHECKED_HOLD_MS`: nothing proves they ended, so a cleared task is not
    /// archived over them (`archive_if_finished`).
    unchecked_recent: usize,
    /// `stale` commands whose output file no longer exists, and those of them started within `UNCHECKED_HOLD_MS`.
    stale_gone: usize,
    stale_gone_recent: usize,
}
/// `delegate_task`'s answer, from its durable result (also on replay).
fn started_line(r: &Value) -> String {
    let id = r["task_id"].as_str().unwrap_or("?");
    let title = crate::text::title(r["title"].as_str().unwrap_or(""));
    let mut model = r["model"].as_str().unwrap_or("").to_owned();
    for (k, v) in r["options"].as_object().into_iter().flatten() {
        model.push_str(&format!(", {k} {}", v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())));
    }
    let how = if r["recovered"] == true { "Recovered the earlier start of" } else { "Started" };
    let place = match (r["worktree"].as_str(), r["branch"].as_str()) {
        (Some(folder), Some(branch)) => format!(" in its own git worktree {folder} (branch {branch})"),
        _ => String::new(),
    };
    format!("{how} '{title}' as task {id} ({model}){place}. Its result arrives as a notice when it is done; end your turn.")
}
fn launch_kind(kind: &str) -> bool {
    matches!(kind, "delegate_task" | "t3_worker_start")
}
/// `$HOME/Work`, the default project folder of a task.
fn default_path() -> Option<String> {
    let path = std::path::Path::new(&std::env::var("HOME").ok()?).join("Vault");
    path.is_dir().then(|| path.to_string_lossy().into_owned())
}
/// The git checkout holding `path`: its top folder and the repository's common git folder (shared by all of a
/// repository's worktrees), both real paths. None outside a repository or when git fails.
async fn checkout(path: &Path) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let run = tokio::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"])
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(Duration::from_secs(5), run).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let mut lines = text.lines();
    let top = std::fs::canonicalize(lines.next()?.trim()).ok()?;
    let common = std::fs::canonicalize(lines.next()?.trim()).ok()?;
    Some((top, common))
}
/// A brief under the default task prompt: the prompt, a rule, then Heimdall's brief.
fn with_preamble(preamble: Option<&str>, task: &str) -> String {
    match preamble {
        Some(p) => format!("{p}\n\n---\n\n{task}"),
        None => task.to_owned(),
    }
}
/// `delegate_task`'s workspace binding in `project` for `path`, the brief to send, and the worktree Heimdall made for
/// the task (its folder and branch), if any. T3 binds a thread to its project's root, to an existing worktree of the
/// project's own repository, or to a new worktree made from that repository; a thread has no other working folder,
/// and T3 provisions a worktree by running git at the project root (bug note 02-50-34: `path` Projects/Heimdall,
/// isolation worktree, failed in T3's "GitWorkflowService.hasCommit (<the Vault>)": the Vault is no
/// repository). So:
/// - isolation worktree, `path` inside the project's own repository (the root is that repository's top): T3's own
///   worktree from it.
/// - isolation worktree, `path` in another repository (a project inside the Vault without a T3 project of its own):
///   Heimdall makes the worktree from the repository holding `path` (`make_worktree`); the thread runs at the root and
///   the brief starts by naming the folder in that worktree. Outside any repository: refused (nothing to isolate).
/// - shared: the root, or the existing worktree of the project's repository that holds `path`. A folder in another
///   repository is never a `worktreePath` (T3 rejects it): the thread runs at the root, told the folder to work in.
async fn workspace(project: &Value, path: &Path, isolate: bool, branch: Option<&Value>, task: &str, title: &str, op: &str) -> Result<(Value, String, Option<(PathBuf, String)>)> {
    let root = std::fs::canonicalize(project["workspaceRoot"].as_str().context("T3 project root missing")?)?;
    let (here, home) = (checkout(path).await, checkout(&root).await);
    let root_repo = home.as_ref().is_some_and(|(top, _)| *top == root);
    let same_repo = root_repo && matches!((&here, &home), (Some((_, a)), Some((_, b))) if a == b);
    let elsewhere = |folder: &Path, how: &str| format!("Working folder: {} ({how}this thread starts in {}; work in the folder named here).\n\n{task}", folder.display(), root.display());
    if isolate {
        if !same_repo {
            let Some((top, common)) = &here else {
                anyhow::bail!("ISOLATION_UNAVAILABLE: {} is not inside a git repository, so it has no worktrees; call delegate_task without isolation", path.display());
            };
            let branch = branch.map(|b| b.as_str().context("branch must be a string")).transpose()?;
            let (folder, branch) = make_worktree(top, common, path, branch, title, op).await?;
            let repo = main_checkout(top, common);
            let brief = elsewhere(&folder, &format!("a new git worktree of {} on branch {branch}, made for this task; ", repo.display()));
            return Ok((json!({"type":"root"}), brief, Some((folder, branch))));
        }
        let mut w = json!({"type":"worktree","baseRef":"HEAD","startFromOrigin":false});
        if let Some(b) = branch {
            w["branch"] = b.clone();
        }
        return Ok((w, task.to_owned(), None));
    }
    if path == root {
        return Ok((json!({"type":"root"}), task.to_owned(), None));
    }
    match &here {
        // Another worktree of the project's own repository.
        Some((top, _)) if same_repo && *top != root => Ok((json!({"type":"existing_worktree","worktreePath":top}), task.to_owned(), None)),
        // A folder of the project's own checkout: the root is that checkout.
        _ if same_repo => Ok((json!({"type":"root"}), task.to_owned(), None)),
        // A folder T3 cannot bind: the thread starts at the root and is told where to work.
        _ => Ok((json!({"type":"root"}), elsewhere(path, ""), None)),
    }
}
/// The main checkout of the repository whose checkout `top` has the common git folder `common` (the folder holding
/// `.git`); `top` itself for a repository without one (a bare repository's worktree).
fn main_checkout(top: &Path, common: &Path) -> PathBuf {
    match (common.file_name(), common.parent()) {
        (Some(name), Some(parent)) if name == ".git" => parent.to_path_buf(),
        _ => top.to_path_buf(),
    }
}
/// A new git worktree of the repository holding `path` (checkout `top`, common git folder `common`): a new branch starts
/// from the HEAD of that checkout, an existing `branch` is checked out as it is, beside the repository's main checkout as `<main>-<branch>` (the Vault's convention: Projects/Heimdall
/// and Projects/Heimdall-agent). The branch is `branch`, or `heimdall/<title>-<id>` from the action id, so a replay of
/// the same call finds the worktree it made and reuses it. Returns the folder matching `path` inside the worktree.
async fn make_worktree(top: &Path, common: &Path, path: &Path, branch: Option<&str>, title: &str, op: &str) -> Result<(PathBuf, String)> {
    let slug = |text: &str| {
        let s: String = text.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
        let s = s.split('-').filter(|p| !p.is_empty()).collect::<Vec<_>>().join("-");
        let s: String = s.chars().take(40).collect();
        s.trim_end_matches('-').to_owned()
    };
    let branch = match branch {
        Some(b) => b.to_owned(),
        None => {
            let name = Some(slug(title)).filter(|s| !s.is_empty()).unwrap_or_else(|| "task".into());
            let digest = crate::body_digest(op);
            let id = digest.strip_prefix("sha256:").unwrap_or(&digest);
            format!("heimdall/{name}-{}", &id[..8.min(id.len())])
        }
    };
    let main = main_checkout(top, common);
    let base = format!("{}-{}", main.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into()), slug(&branch));
    let parent = main.parent().context("the repository has no parent folder")?;
    let inside = path.strip_prefix(top).unwrap_or(Path::new("")).to_path_buf();
    let place = |f: PathBuf| if inside.as_os_str().is_empty() { f } else { f.join(&inside) };
    let git = |args: Vec<std::ffi::OsString>| async move {
        let run = tokio::process::Command::new("git").arg("-C").arg(top).args(&args).kill_on_drop(true).output();
        let out = tokio::time::timeout(Duration::from_secs(120), run).await.context("git timed out")??;
        anyhow::Ok(out)
    };
    // A worktree already on the branch: this call's own (made by an earlier attempt of it), or someone else's.
    let list = git(vec!["worktree".into(), "list".into(), "--porcelain".into()]).await?;
    let mut current: Option<PathBuf> = None;
    for line in String::from_utf8_lossy(&list.stdout).lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(p));
        } else if line.strip_prefix("branch refs/heads/") == Some(branch.as_str()) {
            let at = current.clone().unwrap_or_default();
            // Only the worktree this action made (its id recorded in the worktree's git folder, never in the checkout):
            // another task given the same branch must not share it.
            if at.parent() == Some(parent) && at.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&base))
                && worktree_owner(&at).await.as_deref() == Some(op) {
                return Ok((place(at), branch));
            }
            anyhow::bail!("WORKTREE_BRANCH_IN_USE: branch {branch} is already checked out at {}; pass another branch, or call delegate_task with path {} and no isolation to work there", at.display(), at.display());
        }
    }
    let folder = (1..100)
        .map(|n| parent.join(if n == 1 { base.clone() } else { format!("{base}-{n}") }))
        .find(|f| !f.exists())
        .context("no free folder for the worktree")?;
    let exists = git(vec!["rev-parse".into(), "--verify".into(), "--quiet".into(), format!("refs/heads/{branch}").into()]).await?.status.success();
    let mut args: Vec<std::ffi::OsString> = vec!["worktree".into(), "add".into(), "--quiet".into()];
    if exists {
        args.extend([folder.clone().into_os_string(), branch.clone().into()]);
    } else {
        args.extend(["-b".into(), branch.clone().into(), folder.clone().into_os_string(), "HEAD".into()]);
    }
    let out = git(args).await?;
    if !out.status.success() {
        anyhow::bail!("WORKTREE_FAILED: git worktree add in {} failed: {}", top.display(), String::from_utf8_lossy(&out.stderr).trim());
    }
    let admin = worktree_admin(&folder).await.context("the new worktree has no git folder")?;
    std::fs::write(admin.join(OWNER_FILE), op)?;
    Ok((place(std::fs::canonicalize(&folder)?), branch))
}
/// The file in a worktree's own git folder naming the delegate_task action that made it.
const OWNER_FILE: &str = "heimdall-action";
/// A worktree's own git folder (`<common>/worktrees/<name>`).
async fn worktree_admin(folder: &Path) -> Option<PathBuf> {
    let out = tokio::process::Command::new("git").arg("-C").arg(folder).args(["rev-parse", "--absolute-git-dir"]).kill_on_drop(true).output().await.ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}
/// The action that made the worktree at `folder`, when Heimdall made it.
async fn worktree_owner(folder: &Path) -> Option<String> {
    std::fs::read_to_string(worktree_admin(folder).await?.join(OWNER_FILE)).ok()
}
/// A reply up to this size (about 16k tokens) comes back in one `task_status`; longer ones continue by itemId.
const REPLY_BYTES: usize = 64 * 1024;
/// A run among T3's recent runs that has ended: its status, and the run after it when that one was requested before
/// it ended (queued behind it: a message, or T3's own turn for sub-agents that finished meanwhile, which T3 requests
/// in the same millisecond, 20 Oct 19:18:59.539 before .541; `Workers::started_by_message` tells them apart). None
/// while it runs or when it is no longer among the recent runs.
fn queued_behind(runs: &[Value], run: &str) -> Option<(String, Option<Value>)> {
    let me = runs.iter().find(|x| x["runId"] == run)?;
    let status = me["status"].as_str().filter(|s| terminal(s))?;
    let ended = me["completedAt"].as_str().and_then(at);
    let ordinal = me["ordinal"].as_i64()?;
    let next = runs.iter().filter(|x| x["ordinal"].as_i64().is_some_and(|o| o > ordinal)).min_by_key(|x| x["ordinal"].as_i64());
    let queued = next.filter(|n| n["requestedAt"].as_str().and_then(at).zip(ended).is_some_and(|(q, e)| q <= e));
    Some((status.to_owned(), queued.cloned()))
}
/// An RFC 3339 time as a number that orders like it (nanoseconds; T3 writes milliseconds).
fn at(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso).ok()?.timestamp_nanos_opt()
}
/// The sub-agents a `Subagent "A" finished` or `Subagent "A" and subagent "B" finished` notification names.
fn subagent_names(text: &str) -> Vec<String> {
    let Some(rest) = text.trim().strip_prefix("Subagent ") else { return vec![] };
    let Some(body) = rest.strip_suffix("finished").map(str::trim_end) else { return vec![] };
    body.split('"').skip(1).step_by(2).map(|n| n.trim_end_matches('…').to_owned()).collect()
}
/// Whether a run's first notification is T3 handing finished child work to the parent: the turn it queues
/// (`queue_after_active`) when sub-agents, delegated tasks or the provider's background work report while a turn runs.
/// The installed T3 (0.1.1-nightly 2026-10-19, provider-core `notification.ts` and the provider continuation) writes
/// `Subagent "A" finished` (or failed, was stopped, updated; `Subagent "A" and subagent "B" finished`, mixed kinds
/// led by a sub-agent, `4 subagents finished`), `Delegated task "A" finished` (`2 of 3 delegated tasks finished: A,
/// B`, `… has a question`). `Background activity updated` is its own kind (`start_kind`). Commands, monitors and
/// pull-request watches are not child work of a turn: their turns count as messages.
fn t3_handover(text: &str) -> bool {
    let line = text.lines().next().unwrap_or("").trim();
    let ends = |verbs: &[&str]| verbs.iter().any(|v| line.ends_with(v));
    // `N subagents finished`, `N delegated tasks finished: A, B` or `N of M delegated tasks …`.
    let counted = |noun: &str| {
        let rest = line.trim_start_matches(|c: char| c.is_ascii_digit());
        let rest = rest.strip_prefix(" of ").map(|r| r.trim_start_matches(|c: char| c.is_ascii_digit())).unwrap_or(rest);
        rest.len() < line.len() && rest.strip_prefix(' ').is_some_and(|r| r.starts_with(noun))
    };
    if line.starts_with("Subagent ") || counted("subagents ") {
        return ends(&[" finished", " failed", " was stopped", " were stopped", " updated"]);
    }
    if line.starts_with("Delegated task ") {
        return ends(&[" finished", " failed", " stopped", " has a question"]);
    }
    counted("delegated tasks ")
}
/// What started a run, from its first notification: `t3`, T3 handing finished child work over (`t3_handover`);
/// `activity`, T3's `Background activity updated`, which starts the turn for a Claude Monitor's event and for child work
/// that reported while a turn ran (bug notes 2026-10-24T20-06-04, 2026-10-25T21-22-04: every Monitor tick of a
/// labelling and a download task woke Heimdall); `notification` for the rest (a command, a monitor stopping).
fn start_kind(text: &str) -> &'static str {
    if t3_handover(text) {
        "t3"
    } else if text.lines().next().unwrap_or("").trim() == "Background activity updated" {
        "activity"
    } else {
        "notification"
    }
}
/// The quoted names of a notification's first line with the word before each, lowercase: `Monitor "A" and command
/// "x" finished` is [("monitor", "A"), ("command", "x")] (T3's trailing ellipsis dropped, spaces flattened).
fn quoted_kinds(text: &str) -> Vec<(String, String)> {
    let line = text.lines().next().unwrap_or("");
    let parts: Vec<&str> = line.split('"').collect();
    (1..parts.len()).step_by(2).filter(|&i| i + 1 < parts.len()).map(|i| {
        let lead = parts[i - 1].split_whitespace().last().unwrap_or("").trim_matches(',').to_lowercase();
        (lead, flat(parts[i].trim_end_matches('…')))
    }).collect()
}
/// A `dynamic_tool` item of Claude's Monitor tool (T3 writes the call as pretty JSON, tool name first).
fn monitor_tool(text: &str) -> bool {
    let head: String = text.chars().take(80).filter(|c| !c.is_whitespace()).collect();
    head.starts_with("{\"toolName\":\"Monitor\"")
}
/// A reply with the withheld earlier turns' replies before it, in order (review 21 Oct: the turn that said "Two of
/// three back: A…, B…" was the one that carried A and B).
fn with_earlier(earlier: &[String], last: Option<String>) -> Option<String> {
    let earlier: Vec<&str> = earlier.iter().map(|e| e.trim()).filter(|e| !e.is_empty()).collect();
    if earlier.is_empty() {
        return last;
    }
    let mut parts: Vec<String> = earlier.iter().map(|e| format!("Earlier turn:\n{e}")).collect();
    if let Some(l) = last.as_deref().map(str::trim).filter(|l| !l.is_empty()) {
        parts.push(format!("Last turn:\n{l}"));
    }
    Some(parts.join("\n\n"))
}
/// An activity item that is still going (T3 timeline item statuses).
fn open(state: &str) -> bool {
    matches!(state, "pending" | "running" | "waiting")
}
fn terminal(state: &str) -> bool {
    matches!(
        state,
        "completed" | "failed" | "interrupted" | "cancelled" | "rolled_back"
    )
}
/// The quoted part of a notification `{lead}"…" finished`, without T3's trailing ellipsis.
fn quoted(text: &str, lead: &str) -> Option<String> {
    let rest = text.trim().strip_prefix(lead)?.strip_prefix('"')?;
    let end = rest.rfind('"')?;
    Some(rest[..end].trim_end_matches('…').to_owned())
}
/// The output file Claude names after a background command's id: "… with ID: b1. Output is being written to:
/// /private/tmp/claude-501/…/tasks/b1.output. You will be notified …".
/// Whether a command item's text ends with a background command's own result: a line starting with
/// `BACKGROUND_MARK`, followed by nothing but that result (Claude's one paragraph about the output file).
fn background_tail(text: &str) -> bool {
    text.rfind(BACKGROUND_MARK).is_some_and(|i| (i == 0 || text[..i].ends_with('\n')) && text.len() - i < 600 && !text[i..].trim_end().contains('\n'))
}
fn output_file(text: &str) -> Option<String> {
    const MARK: &str = "Output is being written to: ";
    let rest = &text[text.find(MARK)? + MARK.len()..];
    let end = rest.find(".output").map(|i| i + ".output".len()).or_else(|| rest.find(char::is_whitespace)).unwrap_or(rest.len());
    let path = rest[..end].trim_end_matches('.');
    path.starts_with('/').then(|| path.to_owned())
}
/// Which of `files` (real paths) a process has open, by `lsof` (about 0.2 s; one call for all of them). None when
/// the probe itself failed: then nothing is concluded.
async fn writers(files: &[String]) -> Option<std::collections::HashSet<String>> {
    let run = tokio::process::Command::new("/usr/sbin/lsof").args(["-F", "n", "--"]).args(files).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(5), run).await.ok()?.ok()?;
    // 1 with nothing on stderr: some of the files are open by nobody.
    if !(out.status.success() || (out.status.code() == Some(1) && out.stderr.is_empty())) {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.strip_prefix('n')).map(str::to_owned).collect())
}
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_quotes_and_flat_commands() {
        assert_eq!(quoted("Command \"cd /Users/t/lab/round2; pms…\" finished", "Command ").unwrap(), "cd /Users/t/lab/round2; pms");
        assert_eq!(quoted("Subagent \"Research oMLX drafting+cache\" finished", "Subagent ").unwrap(), "Research oMLX drafting+cache");
        // Real shapes, 20 Oct: one name, two at once, and T3's generic one.
        assert_eq!(subagent_names("Subagent \"Research tiny text LLMs on HF\" finished"), ["Research tiny text LLMs on HF"]);
        assert_eq!(
            subagent_names("Subagent \"TTS and ElevenLabs research\" and subagent \"Name candidates research\" finished"),
            ["TTS and ElevenLabs research", "Name candidates research"]
        );
        assert!(subagent_names("Background activity updated").is_empty() && subagent_names("Command \"x\" finished").is_empty());
        assert!(quoted("Subagent \"x\" finished", "Command ").is_none());
    }
    /// Review 21 Oct: only T3's hand-over of child work is T3's own turn, by the installed T3's exact strings.
    #[test]
    fn t3_handover_is_only_child_work_by_its_real_strings() {
        for own in [
            "Subagent \"Research tiny text LLMs on HF\" finished",
            "Subagent \"TTS and ElevenLabs research\" and subagent \"Name candidates research\" finished",
            "Subagent \"A\", subagent \"B\" and subagent \"C\" failed",
            "Subagent \"Pricing research\" was stopped",
            "Subagent finished",
            "Subagent \"A\" and command \"npm test\" (exit 1) finished",
            "4 subagents finished",
            "2 subagents were stopped",
            "Delegated task \"Benchmark\" finished",
            "Delegated task \"Benchmark\" has a question",
            "2 delegated tasks finished: Benchmark, Review",
            "2 of 3 delegated tasks failed: Benchmark, Review",
        ] {
            assert!(t3_handover(own), "{own}");
        }
        for message in [
            "Command \"cargo test\" (exit 0) finished",
            "2 commands finished",
            "Monitor \"ci\" reported new output",
            "Background task \"x\" finished",
            "#42: checks passed",
            "Subagent \"A\" is running",
            "Please summarize what the subagents found.",
            "Background activity updated soon?",
            "Background activity updated",
            "",
        ] {
            assert!(!t3_handover(message), "{message}");
        }
        assert_eq!(
            quoted_kinds("Subagent \"Review the rubric\" and monitor \"fetch progress…\" finished"),
            [("subagent".to_string(), "Review the rubric".to_string()), ("monitor".to_string(), "fetch progress".to_string())]
        );
        assert_eq!(start_kind("Background activity updated"), "activity");
        assert_eq!(start_kind("Background activity updated\nThe research workers reported."), "activity");
        assert_eq!(start_kind("Background activity updated soon?"), "notification");
        assert_eq!(start_kind("4 subagents finished"), "t3");
        assert_eq!(start_kind("Monitor \"Drama BF16 download final exit or failure only\" was stopped"), "notification");
        assert!(monitor_tool("{\n  \"toolName\": \"Monitor\",\n  \"input\": {"));
        assert!(!monitor_tool("{\n  \"toolName\": \"ToolSearch\",\n  \"input\": {\"query\": \"select:Monitor\"}"));
    }
    #[test]
    fn earlier_turns_ride_before_the_last_in_order() {
        assert_eq!(with_earlier(&[], Some("All done.".into())).as_deref(), Some("All done."));
        assert_eq!(
            with_earlier(&["Two of three back: A is 12, B is 9.".into(), " ".into(), "C is late.".into()], Some("All done: C is 4.".into())).as_deref(),
            Some("Earlier turn:\nTwo of three back: A is 12, B is 9.\n\nEarlier turn:\nC is late.\n\nLast turn:\nAll done: C is 4.")
        );
        assert_eq!(with_earlier(&["A is 12.".into()], None).as_deref(), Some("Earlier turn:\nA is 12."));
        assert_eq!(flat("until grep -q x;\n  do sleep 15; done"), "until grep -q x; do sleep 15; done");
        assert!(open("running") && open("pending") && !open("completed") && !open("failed"));
    }
}
