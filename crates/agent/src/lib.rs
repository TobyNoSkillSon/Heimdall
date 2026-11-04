//! Harness-independent ambient delivery, durable observation spool, and MCP tools.
pub mod clock;
pub mod notice;
/// Emergency mode: the agent failed, a T3 thread covers (docs/emergency.md).
pub mod emergency;
pub mod accounts; // subscription accounts and their quota: which account has quota left, which renews soonest
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod delivery;
pub mod harness;
// Personalise (26 Oct): the configured names in the prompts.
pub mod identity;
pub mod maintenance;
pub mod mcp;
pub mod mcp_contract;
pub mod plain;
pub mod actions;
pub mod protocol;
pub mod reply;
pub mod runtime;
pub mod provider;
pub mod console;
pub mod chatgpt;
mod process_owner;
pub mod session;
pub mod history;
pub mod supervision;
pub mod tool_log;
pub mod view;
/// Scheduled wakes and the delivery heartbeat (selfwake, night 21 Oct).
pub mod wakes;
pub mod taste;
/// Heimdall's own skills: the bundled defaults and Pi's skill folder (docs/skills.md).
pub mod skills;
/// The skills index as Pi shows it; shared with build.rs.
pub mod skill_index;
/// The relay's system prompt: Heimdall's own, composed at build time (docs/claude-relay.md).
pub mod relay_prompt;
pub mod routing;
/// A spent plan limit or prepaid balance as T3 reports a failed run (bug note 2026-11-02T14-47-37).
pub mod limits;
use anyhow::{ensure, Context, Result};
use harness::Harness;
use protocol::{message, Attachment, Event, Observation};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use std::{collections::VecDeque, path::Path};
pub type PayloadArchive = std::sync::Arc<dyn Fn(&Value) -> Result<String> + Send + Sync>;
/// A cache-refresh ping (`Bridge::keep_cache_warm`): never a real request.
pub fn is_cache_ping(wire: &Value) -> bool { wire["cache_ping"] == true }
pub fn body_digest(text:&str)->String { use sha2::Digest; format!("sha256:{:x}",sha2::Sha256::digest(text.as_bytes())) }
fn text_of(e: &Event) -> &str {
    match e { Event::Complete { text, .. } => text, _ => "" }
}
pub use clock::now_ms;
use notice::NoticeKind;
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    WaitingEngine,
    /// The selected API model cannot run yet: no key, a rejected key, or the provider is unreachable.
    WaitingProvider(String),
    ReloadPending,
    Working,
    RateLimited { reset: Option<u64> },
    NeedsReconciliation(String),
    AuthBlocked(String),
    Stopped,
}
impl Status {
    pub fn line(&self) -> String {
        match self {
            Self::ReloadPending => "Agent will reload after this turn".into(),
            Self::WaitingEngine => "Agent waiting for the engine".into(),
            Self::WaitingProvider(reason) => reason.clone(),
            Self::Idle => "Agent ready".into(),
            Self::Working => "Agent responding".into(),
            Self::RateLimited { reset } => format!(
                "Agent rate limited; collecting locally; reset {}",
                reset
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| "not supplied".into())
            ),
            Self::NeedsReconciliation(_) => "Agent checking an interrupted reply".into(),
            Self::AuthBlocked(_) => "The agent is unavailable".into(),
            Self::Stopped => "Agent stopped".into(),
        }
    }
}
#[derive(Debug)]
pub struct MessageRejected;
impl std::fmt::Display for MessageRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f,"A message is too large for the agent context; use a smaller message or retrieve it from the archive")
    }
}
impl std::error::Error for MessageRejected {}
/// Bounded durable data for an interrupted delivery, independent of Pi history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryReview {
    pub id: String,
    pub text: String,
    pub timestamp_ms: Option<u64>,
    pub attachments: Vec<(String, Option<String>)>,
    pub text_truncated: bool,
    pub attachments_truncated: bool,
}
/// How an interrupted or failed delivery resolved itself, for the host to report once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// No tool ran: the same delivery is resent, its speech marked as repeated. Once for most failures; the user's wake
    /// that failed for a passing reason is resent after each of `notice::RETRY_DELAYS_MS` (`Bridge::retry_due`).
    Retrying,
    /// A wake that waited out its retry delay went out again.
    Resent,
    /// No tool ran and the resends failed too: its speech rides the next wake.
    Failed,
    /// A tool may have run: settled as done, never replayed (its calls are in the conversation).
    SettledAfterError,
    /// An abort ended it (the user's Stop, a reload that aborts, the loop guard): settled, never resent.
    Stopped,
    /// The provider refused the request as invalid (a 400 a resend cannot fix): not resent; its speech rides the
    /// next wake without the delivery's images. The reason is in plain words for the island (E10.2).
    Rejected,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub id: String,
    pub outcome: Resolved,
    pub reason: String,
}
pub const REVIEW_ROWS: usize = 32;
pub const REVIEW_TEXT_BYTES: usize = 32 * 1024;
pub const REVIEW_ATTACHMENTS: usize = 32;

/// SQLite keeps collection durable and RAM independent of the unconsumed archive size.
/// One wake is built in memory (8 MiB maximum); later rows stay on disk.
pub struct Bridge<H: Harness = harness::DryRun> {
    pub harness: H,
    db: Connection,
    pub session_id: String,
    pub status: Status,
    reset: Option<u64>,
    active: VecDeque<String>,
    resolutions: Vec<Resolution>,
    /// Deliveries an abort ended (`heimdall_aborted` before their error completion): never resent.
    aborted: std::collections::HashSet<String>,
    clock: clock::ClockHandle,
    /// After a start, before the user's first addressed input: no notice wakes the agent (E1.16, E4.3).
    awaiting_owner: bool,
    /// A start's catch-up and restart message are not in the conversation yet: every wake waits (E4.1).
    start_pending: bool,
    /// When the last notice-only turn went out (E1.18).
    last_notice_turn_ms: Option<u64>,
    /// History from before this start (spool leftovers, queued typed text and notices) for the catch-up (E4.4).
    start_leftovers: Vec<notice::CatchupLine>,
    /// The line before the restart message (a reload's reason).
    start_reason: Option<String>,
    /// Where this start's archived catch-up begins (`catchup_since`); the header names the earliest line.
    start_since: Option<u64>,
    /// Spool observations the pending start's catch-up holds. They stay unsettled until the context is appended
    /// (a failed start keeps them for the next one), and no wake carries them meanwhile (E4.1, E6.1).
    start_claims: std::collections::HashSet<String>,
    /// When Heimdall compacts on its own (`maintenance`); `None`: never (dry run, transports that cannot).
    compaction: Option<maintenance::CompactionPolicy>,
    /// The last wake, typed message, steer, settled turn or start: compactions wait for quiet after it.
    last_activity_ms: u64,
    /// The idle moment (last activity, cache cold) whose history size was already looked at.
    maintenance_checked: Option<(u64, bool)>,
    /// When the last request other than a cache-refresh ping went out (`keep_cache_warm`); `None` until one does.
    last_real_ms: Option<u64>,
    /// Times the user's new words restarted the running turn (`MAX_RESTARTS`); counted from the turn's first wake.
    restarts: u32,
    /// The running agent's history repair (`runtime/history-repair.mjs`) acted in this run: a refusal at its end is
    /// worded "still refuses after repair" (`notice::provider_rejection`). Cleared when the run settles.
    run_repaired: bool,
    /// Emergency mode (docs/emergency.md, `hold_turns`): Heimdall's agent failed and a T3 thread covers. Every input
    /// that would start a turn goes into the conversation as context instead, and to this relay.
    hold: Option<emergency::Relay>,
    /// Idle compaction may run while held: relay mode, whose mirror keeps adding to the conversation and whose
    /// provider works (docs/claude-relay.md). Emergency mode holds it: the provider is down.
    hold_compacts: bool,
}
/// The user's new words restart a turn that has neither acted nor replied at most this often (night review 4); later
/// words steer it, so a long stream of chunks still gets an answer.
pub const MAX_RESTARTS: u32 = 2;
/// Spool keys: when the running compaction began, and where the last finished one's summary ends (`catchup_since`).
const COMPACTION_STARTED: &str = "compaction_started_ms";
const HISTORY_COMPACTED_FROM: &str = "history_compacted_from_ms";
/// Why an utterance's second copy (the same words on another microphone) is kept settled and never rides a wake.
const HEARD_TWICE: &str = "the same words within 3 seconds (heard on another microphone)";
/// Whose words a voice wake carries (night review 9). The decider also sends lines from call or system audio;
/// those wakes go out like any other, but they are not the user addressing Heimdall: they never cancel Heimdall's idle
/// compaction (they wait for it), never restart a running turn (they steer it), do not count as his activity for the
/// compaction's quiet wait, and do not end the wait for his first words after a start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    /// A line from the user's own microphone (his name, a send trigger, the decider, his answer to a question card).
    Owner,
    /// A line from any other source.
    Other,
}
impl<H: Harness> Bridge<H> {
    pub fn open(harness: H, path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_clock(harness, path, clock::system())
    }
    pub fn open_with_clock(harness: H, path: impl AsRef<Path>, clock: clock::ClockHandle) -> Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS observations(seq INTEGER PRIMARY KEY AUTOINCREMENT,id TEXT UNIQUE NOT NULL,text TEXT NOT NULL); CREATE TABLE IF NOT EXISTS deliveries(id TEXT PRIMARY KEY,wire TEXT NOT NULL,query INTEGER NOT NULL,last_seq INTEGER NOT NULL,state TEXT NOT NULL); CREATE TABLE IF NOT EXISTS wake_instructions(id TEXT PRIMARY KEY,instruction TEXT NOT NULL); CREATE TABLE IF NOT EXISTS wake_keys(id TEXT PRIMARY KEY,delivery_id TEXT NOT NULL);")?;
        let has_supplied = db.prepare("PRAGMA table_info(observations)")?.query_map([], |r|r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|name|name=="supplied");
        if !has_supplied {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch("ALTER TABLE observations ADD COLUMN supplied INTEGER NOT NULL DEFAULT 0")?;
            {
                let mut statement = tx.prepare("SELECT wire FROM deliveries WHERE query=0 AND state='completed'")?;
                for wire in statement.query_map([], |r|r.get::<_,String>(0))? {
                    for id in protocol::observation_ids(&serde_json::from_str::<Value>(&wire?)?) {
                        tx.execute("UPDATE observations SET supplied=1 WHERE id=?1", [id])?;
                    }
                }
            }
            tx.commit()?;
        }
        let has_settled = db.prepare("PRAGMA table_info(observations)")?.query_map([], |r|r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|name|name=="settled");
        if !has_settled {
            let tx = db.unchecked_transaction()?;
            tx.execute_batch("ALTER TABLE observations ADD COLUMN settled INTEGER NOT NULL DEFAULT 0;
                CREATE INDEX observations_unsettled ON observations(settled,seq);
                UPDATE observations SET settled=1 WHERE seq<=COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key='cursor'),0)")?;
            // Legacy cursors claimed a contiguous prefix. Restore exact ownership
            // for unresolved wakes, including gaps below a later completed wake.
            let mut stmt = tx.prepare("SELECT id,wire FROM deliveries WHERE query=1 AND state IN ('queued','submitted','echoed','effects_possible','effects_ambiguous','needs_reconciliation')")?;
            for row in stmt.query_map([], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
                let (id, raw) = row?;
                let mut wire: Value = serde_json::from_str(&raw)?;
                let ids = protocol::observation_ids(&wire);
                for observation in &ids {
                    tx.execute("UPDATE observations SET settled=0 WHERE id=?1", [observation])?;
                }
                wire["observation_ids"] = serde_json::json!(ids);
                tx.execute("UPDATE deliveries SET wire=?2 WHERE id=?1", params![id,wire.to_string()])?;
            }
            drop(stmt);
            tx.commit()?;
        }
        let has_at = db.prepare("PRAGMA table_info(observations)")?.query_map([], |r|r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|name|name=="at_ms");
        if !has_at {
            // When each row was heard (wake lines carry it) and why a row left delivery without reaching the model.
            db.execute_batch("ALTER TABLE observations ADD COLUMN at_ms INTEGER; ALTER TABLE observations ADD COLUMN dropped TEXT;")?;
        }
        let has_in_history = db.prepare("PRAGMA table_info(observations)")?.query_map([], |r|r.get::<_,String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?.iter().any(|name|name=="in_history");
        if !has_in_history {
            // Pi took the words in (a request it read, whatever became of its turn): the conversation holds them, so
            // a start's catch-up never sends them again (`begin_start`). A delivery's own record goes when its body
            // is archived (`retire`), so the mark lives on the observation.
            db.execute_batch("ALTER TABLE observations ADD COLUMN in_history INTEGER NOT NULL DEFAULT 0;")?;
        }
        // A crash between writing an append and committing its context ownership
        // cannot prove either delivery or absence. Never replay it blindly.
        db.execute("UPDATE deliveries SET state='needs_reconciliation' WHERE query=0 AND state IN ('submitted','echoed')", [])?;
        let sid = db
            .query_row("SELECT value FROM meta WHERE key='session'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        db.execute("INSERT OR IGNORE INTO meta VALUES('session',?1)", [&sid])?;
        // Upgrade/recovery cannot establish the authority of legacy live wakes.
        // Cancel queued input eagerly, even when an active turn blocks flushing.
        if let Some(epoch) = harness.delivery_epoch() {
            db.execute("UPDATE deliveries SET state='cancelled' WHERE state='queued' AND query=1 AND (json_type(wire,'$.delivery_epoch') IS NOT 'integer' OR json_extract(wire,'$.delivery_epoch')!=?1)", [epoch as i64])?;
        }
        let unresolved=db.query_row("SELECT id FROM deliveries WHERE (query=1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous')) OR state='needs_reconciliation' ORDER BY rowid LIMIT 1",[],|r|r.get::<_,String>(0)).optional()?;
        let stored_reset = db
            .query_row("SELECT value FROM meta WHERE key='rate_reset'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        let reset = stored_reset.as_ref().and_then(|s| s.parse::<u64>().ok());
        let limited = stored_reset.is_some() && reset.map(|s| s > clock.now_ms() / 1000).unwrap_or(true);
        let status = unresolved
            .map(Status::NeedsReconciliation)
            .unwrap_or(if limited {
                Status::RateLimited { reset }
            } else {
                Status::Idle
            });
        let active = {
            let mut stmt = db.prepare("SELECT id FROM deliveries WHERE (query=1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous')) OR state='needs_reconciliation' ORDER BY rowid")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<std::result::Result<VecDeque<_>, _>>()?
        };
        let last_activity_ms = clock.now_ms();
        let mut bridge = Self { harness, db, session_id: sid, status, reset, active, resolutions: Vec::new(), aborted: Default::default(),
            clock, awaiting_owner: false, start_pending: false, last_notice_turn_ms: None, start_leftovers: Vec::new(), start_reason: None, start_since: None,
            start_claims: Default::default(), compaction: None, last_activity_ms, maintenance_checked: None, last_real_ms: None, restarts: 0, run_repaired: false, hold: None, hold_compacts: false };
        bridge.coalesce_queued_wakes()?;
        Ok(bridge)
    }
    /// Upgrade an existing spool with a queue from the affected build. Preserve
    /// source delivery IDs as terminal aliases, so replay cannot duplicate them.
    fn coalesce_queued_wakes(&mut self) -> Result<()> {
        if !self.harness.serial_wakes() { return Ok(()); }
        loop {
            // Find an adjacent ambient pair in delivery order, rather than
            // filtering typed rows out before choosing neighbors. Already
            // coalesced aliases do not split a run; typed entries always do.
            let pair = self.db.query_row(
                "SELECT d.id,n.id FROM deliveries d JOIN wake_instructions w ON w.id=d.id
                 JOIN deliveries n ON n.rowid=(SELECT MIN(rowid) FROM deliveries WHERE rowid>d.rowid AND state!='coalesced')
                 JOIN wake_instructions nw ON nw.id=n.id
                 WHERE d.state='queued' AND n.state='queued'
                 AND json_extract(d.wire,'$.delivery_epoch') IS ?1 AND json_extract(n.wire,'$.delivery_epoch') IS ?1
                 ORDER BY d.rowid LIMIT 1",
                [self.harness.delivery_epoch().map(|e|e as i64)],
                |r| Ok([r.get::<_, String>(0)?, r.get::<_, String>(1)?]),
            ).optional()?;
            let Some(ids) = pair else { break; };
            let load = |id: &str| -> Result<(Value,String,i64)> {
                let (wire,instruction,seq): (String,String,i64) = self.db.query_row("SELECT d.wire,w.instruction,d.last_seq FROM deliveries d JOIN wake_instructions w ON w.id=d.id WHERE d.id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
                Ok((serde_json::from_str(&wire)?,instruction,seq))
            };
            let (mut head, mut instruction, first_seq) = load(&ids[0])?;
            let (tail, next_instruction, last_seq) = load(&ids[1])?;
            if instruction != next_instruction { instruction.push_str("\n\n"); instruction.push_str(&next_instruction); }
            let mut observation_ids = protocol::observation_ids(&head);
            for id in protocol::observation_ids(&tail) { if !observation_ids.contains(&id) { observation_ids.push(id); } }
            head["observation_ids"] = serde_json::json!(observation_ids);
            head["transcript_attempted"] = serde_json::json!(head["transcript_attempted"] != false || tail["transcript_attempted"] != false);
            // The user's words in either make the merged wake his (`Speaker`).
            if tail["from_owner"] != false { head["from_owner"] = serde_json::json!(true); }
            head["message"]["content"].as_array_mut().unwrap().extend(tail["message"]["content"].as_array().unwrap().iter().skip(1).cloned());
            let mut overflow = head["screenshot_overflow"].as_array().cloned().unwrap_or_default();
            for id in tail["screenshot_overflow"].as_array().into_iter().flatten() { if !overflow.contains(id) { overflow.push(id.clone()); } }
            head["screenshot_overflow"] = serde_json::json!(overflow);
            if head["observation_ceiling"].is_i64() && tail["observation_ceiling"].is_i64() {
                head["observation_ceiling"]=serde_json::json!(head["observation_ceiling"].as_i64().unwrap().max(tail["observation_ceiling"].as_i64().unwrap()));
            } else {head.as_object_mut().unwrap().remove("observation_ceiling");}
            protocol::bound_wake_images(&mut head);
            let tx = self.db.unchecked_transaction()?;
            tx.execute("UPDATE deliveries SET wire=?2,last_seq=?3 WHERE id=?1",params![ids[0],head.to_string(),first_seq.max(last_seq)])?;
            tx.execute("UPDATE wake_instructions SET instruction=?2 WHERE id=?1",params![ids[0],instruction])?;
            tx.execute("UPDATE deliveries SET state='coalesced' WHERE id=?1",[&ids[1]])?;
            tx.execute("UPDATE wake_keys SET delivery_id=?1 WHERE delivery_id=?2",params![ids[0],ids[1]])?;
            tx.execute("INSERT OR IGNORE INTO wake_keys VALUES(?1,?2)",params![ids[1],ids[0]])?;
            tx.commit()?;
        }
        Ok(())
    }
    fn blocked(&self) -> bool {
        matches!(
            self.status,
            Status::NeedsReconciliation(_)
                | Status::AuthBlocked(_)
                | Status::WaitingEngine
                | Status::WaitingProvider(_)
                | Status::ReloadPending
        ) || self
            .reset
            .map(|s| self.clock.now_ms() / 1000 < s)
            .unwrap_or(matches!(self.status, Status::RateLimited { reset: None }))
    }
    fn queue_reason(&self) -> Option<String> {
        if self.blocked() { return Some(self.status.line()); }
        if self.start_pending { return Some("Waiting for the start's catch-up".into()); }
        if let Some(reason) = self.harness.input_blocked() { return Some(reason.into()); }
        if self.harness.serial_wakes() && !self.active.is_empty() && !self.harness.supports_steering() {
            return Some("Harness does not support steering an active turn".into());
        }
        None
    }
    /// Persist the reason beside the message, and emit only on a transition.
    fn record_queue(&mut self, id: &str, reason: &str) -> Result<()> {
        let raw: String = self.db.query_row("SELECT wire FROM deliveries WHERE id=?1", [id], |r|r.get(0))?;
        let mut wire: Value = serde_json::from_str(&raw)?;
        if wire["queue_reason"].as_str() != Some(reason) {
            wire["queue_reason"] = serde_json::json!(reason);
            self.db.execute("UPDATE deliveries SET wire=?2 WHERE id=?1", params![id,wire.to_string()])?;
            self.harness.record_queue_fallback(&wire, reason)?;
        }
        Ok(())
    }
    /// The user's wake or typed text queued after `id`.
    fn later_from_owner(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state='queued' AND query=1 AND json_extract(wire,'$.notice') IS NULL AND json_extract(wire,'$.from_owner') IS NOT 0 AND rowid>(SELECT rowid FROM deliveries WHERE id=?1))", [id], |r| r.get(0))?)
    }
    /// When the delivery `id`, queued after a passing failure, is resent; `None` when it is not waiting for one.
    pub fn retry_due(&self, id: &str) -> Result<Option<u64>> {
        Ok(self.db.query_row("SELECT json_extract(wire,'$.retry_at_ms') FROM deliveries WHERE id=?1 AND state='queued'", [id], |r| r.get::<_, Option<i64>>(0)).optional()?.flatten().map(|at| at as u64))
    }
    /// A wake that failed for a passing reason is queued for its resend or on its way again.
    pub fn retry_pending(&self) -> bool {
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state IN ('queued','submitted','echoed') AND json_extract(wire,'$.retry_at_ms') IS NOT NULL)", [], |r| r.get(0)).unwrap_or(false)
    }
    /// The earliest moment a wake waiting out its retry delay goes out (the host wakes its loop for it).
    pub fn retry_deadline(&self) -> Option<u64> {
        let at: Option<i64> = self.db.query_row("SELECT MIN(json_extract(wire,'$.retry_at_ms')) FROM deliveries WHERE state='queued' AND json_extract(wire,'$.retry_at_ms') IS NOT NULL", [], |r| r.get(0)).ok().flatten();
        at.map(|at| at as u64).filter(|at| *at > self.clock.now_ms())
    }
    fn is_queued(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT state='queued' FROM deliveries WHERE id=?1", [id], |r|r.get(0))?)
    }
    fn earlier_queued(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state='queued' AND json_extract(wire,'$.notice') IS NULL AND rowid<(SELECT rowid FROM deliveries WHERE id=?1))", [id], |r|r.get(0))?)
    }
    /// Persist before transporting. Limited or unsupported no-query observations stay local.
    pub async fn observe(&mut self, o: Observation) -> Result<()> {
        if !self.observe_local(&o)? {
            return Ok(());
        }
        if self.harness.observation_append() && !self.blocked() {
            let id = uuid::Uuid::new_v4().to_string();
            let at = if o.timestamp_ms > 1_500_000_000_000 { o.timestamp_ms } else { self.clock.now_ms() };
            let mut wire = message(
                &id,
                &format!("Observation:\n{}", notice::timed(&o.labelled(), at)),
                &[],
                false,
                false,
            )?;
            wire["observation_ids"] = serde_json::json!([o.id]);
            self.deliver(&id, wire, false, 0).await?;
        }
        Ok(())
    }
    /// Persist while the app's immediate off gate is closed, without invoking a harness.
    pub fn observe_local(&mut self, o: &Observation) -> Result<bool> {
        ensure!(
            o.text.len() <= 1_048_576,
            "observation exceeds 1 MiB; archive upstream"
        );
        // The utterance's own wall time when it carries one, else when the harness received it.
        let at = if o.timestamp_ms > 1_500_000_000_000 { o.timestamp_ms } else { self.clock.now_ms() };
        let labelled = o.labelled();
        // One utterance heard on two microphones (both are "Mic"): the second copy is kept with its reason and never
        // rides a wake, so its wake finds nothing new and starts no second delivery (`wake_keyed_through`).
        let twice: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM observations WHERE text=?1 AND id!=?2 AND at_ms BETWEEN ?3 AND ?4)",
            params![labelled, o.id, at.saturating_sub(notice::HEARD_TWICE_MS) as i64, (at + notice::HEARD_TWICE_MS) as i64],
            |r| r.get(0),
        )?;
        if twice {
            self.db.execute(
                "INSERT OR IGNORE INTO observations(id,text,at_ms,settled,dropped) VALUES(?1,?2,?3,1,?4)",
                params![o.id, labelled, at as i64, HEARD_TWICE],
            )?;
            return Ok(false);
        }
        let inserted = self.db.execute(
            "INSERT OR IGNORE INTO observations(id,text,at_ms) VALUES(?1,?2,?3)",
            params![o.id, labelled, at as i64],
        )?;
        Ok(inserted != 0)
    }
    async fn deliver(&mut self, id: &str, wire: Value, query: bool, last_seq: i64) -> Result<()> {
        self.deliver_permitted(id, wire, query, last_seq, None)
            .await
    }
    async fn deliver_permitted(
        &mut self,
        id: &str,
        mut wire: Value,
        query: bool,
        last_seq: i64,
        permit: Option<delivery::DeliveryPermit>,
    ) -> Result<()> {
        let real = query && !is_cache_ping(&wire);
        if query { wire["transcript_attempted"] = serde_json::json!(true); }
        if query && self.harness.supports_steering() { wire["priority"] = serde_json::json!("now"); }
        self.db.execute(
            "INSERT OR IGNORE INTO deliveries VALUES(?1,?2,?3,?4,'queued')",
            params![id, wire.to_string(), query, last_seq],
        )?;
        self.db.execute("UPDATE deliveries SET wire=?2 WHERE id=?1", params![id,wire.to_string()])?;
        // The user's words while the running turn has neither acted nor replied restart that turn with them (night pass
        // H2): a steer waits for the request in flight and then gets an answer of its own, so 19 Oct 18:39 a question
        // said twice was answered twice. Notices never reach a running turn (they wait for settlement); a wake for
        // someone else's line steers; after `MAX_RESTARTS` restarts his words steer too (night review 4). A turn's
        // restarts count from its first wake.
        if query && (self.active.is_empty() || !self.harness.turn_active()) { self.restarts = 0; }
        if query && wire["notice"].is_null() && wire["from_owner"] != false && self.restarts < MAX_RESTARTS
            && self.harness.can_rejoin() && self.harness.rejoin().await? {
            self.restarts += 1;
        }
        // Mark BEFORE writing: a broken pipe can still mean bytes reached the harness.
        self.db
            .execute("UPDATE deliveries SET state='submitted' WHERE id=?1", [id])?;
        let scoped_notice = wire["delivery_epoch"].is_u64() || (query && self.harness.delivery_epoch().is_some());
        let mut wire = wire;
        let ids = protocol::observation_ids(&wire);
        let has_observations = !ids.is_empty();
        wire["observation_ids"] = serde_json::json!(ids);
        if let Some(parts) = wire["message"]["content"].as_array_mut().filter(|_| has_observations) {
            for part in parts { if let Some(text) = part["text"].as_str() { part["text"] = Value::String(protocol::compact_transcript(text)); } }
        }
        protocol::describe_overflow_images(&mut wire);
        let result = match permit {
            Some(p) => self.harness.send_with_permit(wire, p).await,
            None => self.harness.send(wire).await,
        };
        if let Err(e) = result {
            if e.downcast_ref::<delivery::DeliveryBlocked>()
                .is_some_and(|off| off.written == 0)
            {
                self.db.execute(
                    "UPDATE deliveries SET state=?2 WHERE id=?1",
                    params![id, if scoped_notice { "cancelled" } else { "queued" }],
                )?;
            } else if let Some(blocked) = e.downcast_ref::<harness::InputUnavailable>() {
                self.db.execute("UPDATE deliveries SET state='queued' WHERE id=?1", [id])?;
                self.record_queue(id, blocked.0)?;
                return Ok(());
            } else if e.downcast_ref::<harness::EngineUnavailable>().is_some() {
                self.db
                    .execute("UPDATE deliveries SET state='queued' WHERE id=?1", [id])?;
                self.status = Status::WaitingEngine;
                self.record_queue(id, "Agent waiting for the engine")?;
            } else {
                self.db.execute(
                    "UPDATE deliveries SET state='needs_reconciliation' WHERE id=?1",
                    [id],
                )?;
                if query && !self.active.iter().any(|a| a == id) {
                    self.active.push_back(id.into());
                }
                self.status = Status::NeedsReconciliation(id.into());
            }
            return Err(e);
        }
        if !query {
            let tx = self.db.unchecked_transaction()?;
            for observation in &ids { tx.execute("UPDATE observations SET supplied=1 WHERE id=?1", [observation])?; }
            tx.execute("UPDATE deliveries SET state='appended' WHERE id=?1", [id])?;
            tx.commit()?;
        }
        if query && !self.active.iter().any(|a| a == id) {
            self.active.push_back(id.into());
        }
        if real { self.last_real_ms = Some(self.clock.now_ms()); }
        Ok(())
    }
    /// Wakes carry ordered observations not owned by another unresolved delivery.
    pub async fn wake(
        &mut self,
        instruction: &str,
        images: &[Attachment],
        urgent: bool,
    ) -> Result<Option<String>> {
        self.wake_keyed(None, instruction, images, urgent, None)
            .await
    }
    /// An outbox notice uses a stable id and its original stop epoch. Repeated
    /// callbacks never enqueue it twice, including after process/app recovery.
    pub async fn wake_keyed(
        &mut self,
        key: Option<&str>,
        instruction: &str,
        images: &[Attachment],
        urgent: bool,
        permit: Option<delivery::DeliveryPermit>,
    ) -> Result<Option<String>> {
        self.wake_keyed_through(key,instruction,images,urgent,permit,None,Speaker::Owner).await
    }
    /// A background decision owns only the observations in its captured sentence.
    /// Preserve that ceiling through queued delivery and ambient coalescing. `speaker`: whose line the wake was sent
    /// for (`Speaker`).
    #[allow(clippy::too_many_arguments)]
    pub async fn wake_keyed_through(
        &mut self, key:Option<&str>, instruction:&str, images:&[Attachment], urgent:bool,
        permit:Option<delivery::DeliveryPermit>, observation:Option<&str>, speaker:Speaker,
    ) -> Result<Option<String>> {
        let ceiling=observation.map(|id|self.db.query_row("SELECT seq FROM observations WHERE id=?1",[id],|r|r.get::<_,i64>(0))).transpose()?.unwrap_or(i64::MAX);
        let epoch = permit.as_ref().map(|p|p.generation()).or_else(||self.harness.delivery_epoch());
        if let Some(p) = &permit {
            p.check()?;
        }
        // Pending context older than the window stays in the archive instead of riding this wake (root cause 4).
        self.drop_stale_context()?;
        // An uncertain delivery (between a crash and the session check) queues this wake; it never fails it.
        let (text, last_seq, ids) = self.build_transcript_through(instruction,ceiling,None)?;
        // One wake per utterance (bug note 21 Oct 00:10:14): a wake for words the agent already has — another
        // delivery carries them, or they were the same words heard on a second microphone — with nothing new and no
        // image starts no second delivery (it would restart or steer the running turn with a repeat). Words a start's
        // catch-up holds (a start began between their observation and this wake) are only context: the wake still
        // goes out, after the start context, so the user's request starts a turn (night 3 review).
        if let Some(o) = observation.filter(|_| ids.is_empty() && images.is_empty()) {
            let with_agent: bool = self.db.query_row(
                "SELECT dropped IS ?2 OR EXISTS(SELECT 1 FROM deliveries d,json_each(d.wire,'$.observation_ids') j
                    WHERE d.query=1 AND j.value=?1 AND d.state!='cancelled') FROM observations WHERE id=?1",
                params![o, HEARD_TWICE], |r| r.get(0))?;
            if with_agent {
                return Ok(None);
            }
        }
        let id = key
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if self
            .db
            .query_row("SELECT 1 FROM deliveries WHERE id=?1 UNION ALL SELECT 1 FROM wake_keys WHERE id=?1", [&id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some()
        {
            return Ok(None);
        }
        // The user addressed Heimdall (a repeated wake is not new input): notices may wake it again (E1.16) and an idle
        // compaction gives way. A wake for someone else's line does neither (`Speaker`, night review 9).
        let from_owner = speaker == Speaker::Owner;
        if from_owner {
            self.awaiting_owner = false;
            self.owner_input().await?;
        }
        if self.hold.is_some() {
            self.hold_input(&id, "wake", &text, images, &ids, last_seq, epoch).await?;
            return Ok(None);
        }
        let mut wire = message(&id, &text, images, true, urgent)?;
        wire["timestamp_ms"] = serde_json::json!(self.clock.now_ms());
        wire["transcript_attempted"] = serde_json::json!(false);
        wire["from_owner"] = serde_json::json!(from_owner);
        wire["observation_ids"] = serde_json::json!(ids);
        if observation.is_some() { wire["observation_ceiling"]=serde_json::json!(ceiling); }
        // Direct live bridge callers capture the current epoch at the wake
        // decision too; outer callers carry the earlier pipeline permit.
        if let Some(epoch) = epoch { wire["delivery_epoch"] = serde_json::json!(epoch); }
        if let Some(p) = &permit {
            wire["delivery_epoch"] = serde_json::json!(p.generation());
        }
        // Coalesce only a genuinely unavailable transport's pending ambient run.
        // Active, steerable turns receive new wakes immediately; typed rows remain
        // ordering barriers in any fallback backlog.
        let pending: Option<(String, String, String)> = if self.harness.serial_wakes() {
            // Only the tail of the contiguous ambient run is eligible. A
            // later typed entry (even explicit steering already submitted)
            // is an ordering barrier, never something to merge across.
            self.db.query_row("SELECT d.id,d.wire,w.instruction FROM deliveries d JOIN wake_instructions w ON w.id=d.id WHERE d.state='queued' AND json_extract(d.wire,'$.delivery_epoch') IS ?1 AND d.rowid=(SELECT MAX(rowid) FROM deliveries WHERE state!='coalesced')", [epoch.map(|e|e as i64)], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?
        } else { None };
        if let Some((pending_id, old_wire, old_instruction)) = pending {
            let instruction = if old_instruction == instruction { old_instruction } else { format!("{old_instruction}\n\n{instruction}") };
            let mut merged: Value = serde_json::from_str(&old_wire)?;
            // The user's new words in a wake waiting to be resent make it his new request: it goes now, with resends of
            // its own, and a start resumes it like any fresh request (`begin_start`) instead of making it history.
            if from_owner {
                merged["from_owner"] = serde_json::json!(true);
                if let Some(o) = merged.as_object_mut() { for k in ["auto_retry", "retries", "retry_at_ms", "failed_at_ms"] { o.remove(k); } }
                merged["timestamp_ms"] = serde_json::json!(self.clock.now_ms());
            }
            let ceiling=if observation.is_some() {ceiling.max(merged["observation_ceiling"].as_i64().unwrap_or(i64::MAX))} else {i64::MAX};
            let (text, seq, ids) = self.build_transcript_through(&instruction,ceiling,Some(&pending_id))?;
            merged["message"]["content"][0]["text"] = Value::String(text);
            merged["observation_ids"] = serde_json::json!(ids);
            merged["message"]["content"].as_array_mut().unwrap().extend(wire["message"]["content"].as_array().unwrap().iter().skip(1).cloned());
            if ceiling<i64::MAX { merged["observation_ceiling"]=serde_json::json!(ceiling); }
            else {merged.as_object_mut().unwrap().remove("observation_ceiling");}
            protocol::bound_wake_images(&mut merged);
            let tx = self.db.unchecked_transaction()?;
            tx.execute("INSERT INTO wake_keys VALUES(?1,?2)",params![id,pending_id])?;
            tx.execute("UPDATE wake_instructions SET instruction=?2 WHERE id=?1",params![pending_id,instruction])?;
            tx.execute("UPDATE deliveries SET wire=?2,last_seq=?3 WHERE id=?1",params![pending_id,merged.to_string(),seq])?;
            tx.commit()?;
            if let Some(reason) = self.queue_reason() { self.record_queue(&pending_id, &reason)?; }
            else { self.flush_queued().await?; }
            return Ok(None);
        }
        protocol::bound_wake_images(&mut wire);
        self.db.execute(
            "INSERT INTO wake_instructions VALUES(?1,?2)",
            params![id, instruction],
        )?;
        self.db.execute(
            "INSERT INTO deliveries VALUES(?1,?2,1,?3,'queued')",
            params![id, wire.to_string(), last_seq],
        )?;
        if let Some(reason) = self.queue_reason() { self.record_queue(&id, &reason)?; return Ok(None); }
        if self.earlier_queued(&id)? { self.record_queue(&id, "Waiting for an earlier undelivered message")?; self.flush_queued().await?; }
        else { self.deliver_permitted(&id, wire, true, last_seq, permit).await?; }
        if self.is_queued(&id)? { return Ok(None); }
        self.status = Status::Working;
        Ok(Some(id))
    }
    /// Typed messages/notices are their own durable input, not another voice
    /// wake: they do not silently attach or acknowledge pending recorded speech.
    /// Late attachments for the turn that is running (E9.1): one steer carrying `text` and the images, delivered to
    /// Pi as a steering prompt. The caller checks that a turn is running; this never starts one by itself.
    pub async fn steer(
        &mut self,
        text: &str,
        images: &[Attachment],
        permit: Option<delivery::DeliveryPermit>,
    ) -> Result<Option<String>> {
        if let Some(p) = &permit {
            p.check()?;
        }
        self.owner_input().await?;
        let id = uuid::Uuid::new_v4().to_string();
        let mut wire = message(&id, text, images, true, false)?;
        if let Some(p) = &permit {
            wire["delivery_epoch"] = serde_json::json!(p.generation());
        }
        self.deliver_permitted(&id, wire, true, 0, permit).await?;
        Ok(Some(id))
    }
    /// The user addressed Heimdall: compactions wait for quiet again from now. A compaction already running is never
    /// cancelled for him ("When the agent is compacting I should not be able to interrupt you"; a cancelled
    /// summary still bills its input): his words queue behind it like any input and go out as it ends.
    async fn owner_input(&mut self) -> Result<()> {
        self.last_activity_ms = self.clock.now_ms();
        Ok(())
    }
    /// The policy `maintain` applies (`maintenance::CompactionPolicy`); the host sets it for each started agent.
    pub fn set_compaction_policy(&mut self, policy: Option<maintenance::CompactionPolicy>) {
        self.compaction = policy;
        self.maintenance_checked = None;
    }
    /// The policy the host set for this start, if any.
    pub fn compaction_policy(&self) -> Option<maintenance::CompactionPolicy> {
        self.compaction
    }
    /// Starts a compaction while the agent is idle and the user has been quiet, when the history is near Pi's own
    /// threshold or the provider's cache for it has gone cold (`maintenance`). Returns why it started one. Looks at
    /// the history size once per idle moment; call it after every event and on a timer.
    pub async fn maintain(&mut self) -> Result<Option<&'static str>> {
        // A compaction is a provider request: none while the agent is down (emergency mode); relay mode compacts.
        if self.hold.is_some() && !self.hold_compacts { return Ok(None); }
        let Some(policy) = self.compaction else { return Ok(None) };
        if self.blocked() || self.start_pending || self.harness.turn_active() || self.harness.input_blocked().is_some() {
            return Ok(None);
        }
        // Queued input goes out first: the user's words and his clicks. Notices that wait for him (a question held until he
        // addresses Heimdall) or for the notice gap do not hold a compaction back (night review 9); they go out after it.
        if self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state='queued' AND (json_extract(wire,'$.notice') IS NULL OR json_extract(wire,'$.notice')='answer'))", [], |r| r.get::<_, bool>(0))? {
            return Ok(None);
        }
        let now = self.clock.now_ms();
        let cold = policy.cache_cold(self.harness.last_request(), now);
        let quiet = now >= self.last_activity_ms.saturating_add(policy.quiet_ms);
        if !quiet || self.maintenance_checked == Some((self.last_activity_ms, cold)) {
            return Ok(None);
        }
        self.maintenance_checked = Some((self.last_activity_ms, cold));
        let Some((Some(tokens), _)) = self.harness.context_tokens().await? else { return Ok(None) };
        let reason = policy.reason(tokens, quiet, cold);
        if reason.is_some() {
            self.harness.compact_history().await?;
        }
        Ok(reason)
    }
    /// Keeps the provider's hour cache warm (`maintenance::KeepWarm`): once the last request is 55 minutes old and
    /// the agent is idle, sends one short cache-refresh message the agent answers with a stall check or `NO_REPLY`.
    /// `agent_on` is the host's Agent toggle. Returns the ping's delivery id when one went out. Call it on a timer.
    pub async fn keep_cache_warm(&mut self, agent_on: bool) -> Result<Option<String>> {
        if self.hold.is_some() { return Ok(None); }
        let Some(warm) = self.compaction.and_then(|p| p.warm) else { return Ok(None) };
        if self.blocked() || self.start_pending || self.turn_running() || self.harness.input_blocked().is_some() {
            return Ok(None);
        }
        // Queued input goes first (the same rule as `maintain`); notices waiting for the user or the notice gap do not hold
        // a ping back.
        if self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state='queued' AND (json_extract(wire,'$.notice') IS NULL OR json_extract(wire,'$.notice')='answer'))", [], |r| r.get::<_, bool>(0))? {
            return Ok(None);
        }
        let last = self.harness.last_request();
        // After a start, the conversation's last request stands in until a real one goes out. It is taken once: the
        // pings' own requests move `last_request` and must never move the 3-hour window.
        if self.last_real_ms.is_none() { self.last_real_ms = last.map(|l| l.at_ms); }
        let Some(last_real) = self.last_real_ms else { return Ok(None) };
        if !warm.due(agent_on, last, last_real, self.clock.now_ms()) {
            return Ok(None);
        }
        let id = format!("cache-ping-{}", uuid::Uuid::new_v4());
        // After a start, until the user talks, the agent does nothing on its own (the restart message): the ping asks only
        // for NO_REPLY then.
        let text = if self.awaiting_owner { maintenance::PING_TEXT_AFTER_START } else { maintenance::PING_TEXT };
        let mut wire = message(&id, text, &[], true, false)?;
        wire["timestamp_ms"] = serde_json::json!(self.clock.now_ms());
        wire["cache_ping"] = serde_json::json!(true);
        wire["from_owner"] = serde_json::json!(false);
        if let Some(epoch) = self.harness.delivery_epoch() { wire["delivery_epoch"] = serde_json::json!(epoch); }
        let status = self.status.clone();
        if let Err(e) = self.deliver(&id, wire, true, 0).await {
            // A ping that failed to go out is dropped: it never holds the agent in review or reaches the user.
            self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [&id])?;
            self.active.retain(|a| a != &id);
            self.status = status;
            return Err(e);
        }
        // A ping that could not go out now is pointless later.
        if self.is_queued(&id)? {
            self.db.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
            return Ok(None);
        }
        Ok(Some(id))
    }
    fn is_ping_delivery(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT json_extract(wire,'$.cache_ping') IS 1 FROM deliveries WHERE id=?1", [id], |r| r.get::<_, bool>(0)).optional()?.unwrap_or(false))
    }
    pub async fn send_text(
        &mut self,
        key: Option<&str>,
        text: &str,
        urgent: bool,
        permit: Option<delivery::DeliveryPermit>,
    ) -> Result<Option<String>> {
        self.send_text_with_images(key, text, &[], urgent, permit).await
    }
    /// A typed message with images the model sees (a drop onto the notch: app/src/drops.rs).
    pub async fn send_text_with_images(
        &mut self,
        key: Option<&str>,
        text: &str,
        images: &[Attachment],
        urgent: bool,
        permit: Option<delivery::DeliveryPermit>,
    ) -> Result<Option<String>> {
        let epoch = permit.as_ref().map(|p|p.generation()).or_else(||self.harness.delivery_epoch());
        if let Some(p) = &permit {
            p.check()?;
        }
        if text.len() > self.harness.max_text_bytes() {
            return Err(MessageRejected.into());
        }
        let id = key
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if self
            .db
            .query_row("SELECT 1 FROM deliveries WHERE id=?1", [&id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_some()
        {
            return Ok(None);
        }
        let mut wire = message(&id, text, images, true, urgent)?;
        protocol::bound_wake_images(&mut wire);
        wire["timestamp_ms"] = serde_json::json!(self.clock.now_ms());
        if let Some(epoch) = epoch { wire["delivery_epoch"] = serde_json::json!(epoch); }
        if let Some(p) = &permit {
            wire["delivery_epoch"] = serde_json::json!(p.generation());
        }
        // A typed message is the user addressing Heimdall (notices use `notice`).
        self.awaiting_owner = false;
        self.owner_input().await?;
        if self.hold.is_some() {
            self.hold_input(&id, "typed", text, images, &[], 0, epoch).await?;
            return Ok(None);
        }
        self.db.execute(
            "INSERT INTO deliveries VALUES(?1,?2,1,0,'queued')",
            params![id, wire.to_string()],
        )?;
        if let Some(reason) = self.queue_reason() { self.record_queue(&id, &reason)?; return Ok(None); }
        if self.earlier_queued(&id)? { self.record_queue(&id, "Waiting for an earlier undelivered message")?; self.flush_queued().await?; }
        else { self.deliver_permitted(&id, wire, true, 0, permit).await?; }
        if self.is_queued(&id)? { return Ok(None); }
        self.status = Status::Working;
        Ok(Some(id))
    }
    fn build_transcript_through(&self, instruction: &str, ceiling: i64, owner: Option<&str>) -> Result<(String, i64, Vec<String>)> {
        let limit = self.harness.max_text_bytes();
        if instruction.len() + 2048 >= limit {
            return Err(MessageRejected.into());
        }
        let data_limit = (limit - instruction.len() - 2048).min(8 * 1024 * 1024);
        // Retry payloads carry speech again even if Pi consumed it previously:
        // compaction can remove it at any time, including after an inspection.
        // Normal wakes still reference durable no-query appends exactly once.
        // A fresh fallback queue has not reached the harness yet. Missing
        // attempt metadata in a legacy queue is treated conservatively as retry.
        let resend = owner.map(|id| self.db.query_row(
            "SELECT json_extract(wire,'$.transcript_attempted') IS NOT 0 FROM deliveries WHERE id=?1",
            [id], |r| r.get::<_, bool>(0))).transpose()?.unwrap_or(false);
        let mut text = String::from("Transcript:\n");
        if resend { text.push_str("(resent after a delivery interruption; ignore repeated speech)\n"); }
        let mut last_seq = 0;
        let mut ids = Vec::new();
        let mut referenced = false;
        let mut data_bytes = text.len();
        {
            let mut stmt = self
                .db
                .prepare("SELECT seq,text,id,supplied,at_ms FROM observations o WHERE settled=0 AND seq<=?1
                    AND NOT EXISTS(SELECT 1 FROM deliveries d,json_each(d.wire,'$.observation_ids') j
                        WHERE d.query=1 AND d.id IS NOT ?2 AND j.value=o.id
                        AND d.state IN ('queued','submitted','echoed','effects_possible','effects_ambiguous','needs_reconciliation'))
                    ORDER BY seq")?;
            let rows = stmt.query_map(params![ceiling,owner], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, bool>(3)?, r.get::<_, Option<i64>>(4)?))
            })?;
            for row in rows {
                let (seq, t, id, supplied, at) = row?;
                // A pending start's catch-up carries this line; the wake must not repeat it (E6.1).
                if self.start_claims.contains(&id) { continue; }
                let t = protocol::compact_transcript(&t);
                // Every line says when it was heard: `[17:52:38 Mic · User] words` (root cause 4).
                let t = match at { Some(at) if t.starts_with('[') => notice::timed(&t, at as u64), _ => t };
                if data_bytes + t.len() > data_limit { break; }
                data_bytes += t.len() + 1;
                if supplied && self.harness.observation_append() && !resend { referenced = true; }
                else { text.push_str(&t); text.push('\n'); }
                last_seq = seq;
                ids.push(id);
            }
        }
        if referenced { text.push_str("Use the observations already in context.\n"); }
        text.push_str("\nOwner's wake instruction:\n");
        text.push_str(instruction);
        ensure!(text.len() <= 9 * 1024 * 1024, "wake instruction too large");
        let waiting: Vec<String> = self.db.prepare("SELECT id FROM observations WHERE settled=0 AND seq>?1")?
            .query_map([last_seq], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let remaining = waiting.iter().filter(|id| !self.start_claims.contains(*id)).count();
        if remaining > 0 {
            text.push_str(&format!("\n[Backlog: {remaining} additional observations retained locally for a later batch.]\n"));
        }
        Ok((text, last_seq, ids))
    }
    pub async fn next(&mut self) -> Result<Option<Event>> {
        let e = match self.harness.next().await {
            Ok(e) => e,
            Err(e) => {
                self.status = if e.downcast_ref::<harness::EngineUnavailable>().is_some() {
                    Status::WaitingEngine
                } else {
                    Status::NeedsReconciliation(
                        self.active
                            .front()
                            .cloned()
                            .unwrap_or_else(|| e.to_string()),
                    )
                };
                return Err(e);
            }
        };
        if let Some(ref event) = e {
            self.handle(event)?;
        } else if let Some(id) = self.active.front() {
            self.status = Status::NeedsReconciliation(id.clone());
        }
        Ok(e)
    }
    pub fn handle(&mut self, e: &Event) -> Result<()> {
        // A settled turn is activity: compactions wait for quiet after it (`maintain`). A compaction that finished
        // ends the backoff its cancelled predecessors built up.
        match e {
            Event::Complete { query: true, .. } => self.last_activity_ms = self.clock.now_ms(),
            // A finished compaction holds everything said before it began: a later start's catch-up begins there
            // ("after the compaction … what we need to send is any audio that happened after the
            // compaction started").
            Event::Other(v) if v["type"] == "compaction_start" => self.set_meta(COMPACTION_STARTED, self.clock.now_ms())?,
            // Pi read this delivery's words: they are in the conversation from now on, even if its turn fails.
            Event::Consumed { id } => self.mark_in_history(id)?,
            Event::Other(v) if v["type"] == "heimdall_history_repair" && v["state"] != "repaired_at_start" => self.run_repaired = true,
            // After the run's completions (the harness reports them first).
            Event::Other(v) if v["type"] == "agent_settled" => self.run_repaired = false,
            Event::Other(v) if v["type"] == "compaction_end" && v["aborted"] != true && v["errorMessage"].is_null() => {
                let began = self.meta_u64(COMPACTION_STARTED)?.unwrap_or_else(|| self.clock.now_ms());
                self.set_meta(HISTORY_COMPACTED_FROM, began)?;
            }
            _ => {}
        }
        match e {
            Event::Deferred { id, reason } => {
                let state: Option<String> = self
                    .db
                    .query_row("SELECT state FROM deliveries WHERE id=?1", [id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if state
                    .as_deref()
                    .is_some_and(|s| matches!(s, "submitted" | "echoed"))
                {
                    // A deferred ping is pointless later: dropped, never queued.
                    let ping = self.is_ping_delivery(id)?;
                    self.db
                        .execute("UPDATE deliveries SET state=?2 WHERE id=?1", params![id, if ping { "cancelled" } else { "queued" }])?;
                    self.active.retain(|a| a != id);
                    if !ping { self.record_queue(id, reason)?; }
                    if !matches!(self.status, Status::NeedsReconciliation(_)) {
                        self.status = if reason == "Agent waiting for the engine" { Status::WaitingEngine }
                            else if self.active.is_empty() { Status::Idle } else { Status::Working };
                    }
                } else if state.as_deref().is_some_and(|s| matches!(s, "effects_possible" | "effects_ambiguous" | "needs_reconciliation")) {
                    // The engine went away after a tool may have run: never replay it.
                    self.settle_after_error(id, reason)?;
                    self.refresh_unresolved()?;
                }
            }
            Event::Echo { id } => {
                self.db.execute(
                    "UPDATE deliveries SET state='echoed' WHERE id=?1 AND state='submitted'",
                    [id],
                )?;
            }
            Event::Complete {
                id, query, error, ..
            } => {
                let id = id
                    .as_ref()
                    .or(if *query { self.active.front() } else { None })
                    .cloned();
                if let Some(id) = &id {
                    let row = self
                        .db
                        .query_row(
                            "SELECT query,state FROM deliveries WHERE id=?1",
                            [id],
                            |r| {
                                Ok((
                                    r.get::<_, bool>(0)?,
                                    r.get::<_, String>(1)?,
                                ))
                            },
                        )
                        .optional()?;
                    let aborted = self.aborted.remove(id);
                    if let Some((wake, delivery_state)) = row {
                        let ambiguous_effect = matches!(e, Event::Complete { id: None, .. })
                            && delivery_state == "effects_ambiguous";
                        let untouched = matches!(delivery_state.as_str(), "submitted" | "echoed");
                        if *error && (!wake || (untouched && self.is_ping_delivery(id)?)) {
                            // A failed cache-refresh ping is never resent, queued or reported to the user.
                            self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [id])?;
                        } else if *error && aborted && (untouched || matches!(delivery_state.as_str(), "effects_possible" | "effects_ambiguous" | "needs_reconciliation")) {
                            // Ended on purpose: never auto-retried, its speech settled with it.
                            self.settle_after_error(id, text_of(e))?;
                            if let Some(last) = self.resolutions.last_mut() { last.outcome = Resolved::Stopped; }
                        } else if *error && untouched && matches!(self.status, Status::RateLimited { .. }) {
                            self.db.execute("UPDATE deliveries SET state='queued' WHERE id=?1", [id])?;
                            self.active.retain(|a| a != id);
                        } else if *error && untouched && wake && notice::provider_rejection(text_of(e), self.run_repaired).is_some() {
                            // The provider refused this request as invalid: the same request fails again, so it is
                            // not resent; its speech rides the next wake once, without this delivery's images.
                            self.reject(id, text_of(e))?;
                        } else if *error && untouched {
                            // Nothing ran: resend once, then hand the speech to the next wake.
                            self.retry(id, text_of(e))?;
                        } else if *error && !matches!(delivery_state.as_str(), "effects_possible" | "effects_ambiguous" | "needs_reconciliation") {
                            // Already resolved (or queued again): a late or repeated failure changes nothing.
                        } else if *error || ambiguous_effect {
                            // A tool may have run (or the attribution is ambiguous): settle, never replay.
                            self.settle_after_error(id, if *error { text_of(e) } else { "" })?;
                            if !*error { self.resolutions.pop(); }
                        } else {
                            let tx = self.db.transaction()?;
                            tx.execute(
                                "UPDATE deliveries SET state='completed' WHERE id=?1",
                                [id],
                            )?;
                            if wake {
                                Self::settle_observations(&tx, id)?;
                            }
                            tx.commit()?;
                        }
                        if wake {
                            self.active.retain(|a| a != id);
                            // A different delivery's outcome cannot clear an unresolved one.
                            if !matches!(
                                self.status,
                                Status::RateLimited { .. }
                                    | Status::NeedsReconciliation(_)
                                    | Status::AuthBlocked(_)
                                    | Status::ReloadPending
                            ) {
                                self.status = if self.active.is_empty() { Status::Idle } else { Status::Working };
                            }
                            self.refresh_unresolved()?;
                        }
                    }
                }
            }
            Event::Limit {
                rejected: true,
                reset,
                ..
            } => {
                self.db.execute("INSERT INTO meta VALUES('rate_reset',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[reset.map(|s|s.to_string()).unwrap_or_else(||"unknown".into())])?;
                self.reset = *reset;
                if !matches!(
                    self.status,
                    Status::NeedsReconciliation(_) | Status::AuthBlocked(_)
                ) {
                    self.status = Status::RateLimited { reset: *reset };
                }
            }
            Event::Limit {
                rejected: false, ..
            } if matches!(self.status, Status::RateLimited { reset: None }) => {
                self.db
                    .execute("DELETE FROM meta WHERE key='rate_reset'", [])?;
                self.reset = None;
                self.status = Status::Idle;
            }
            Event::Other(v) if v["type"] == "heimdall_aborted" => {
                if let Some(id) = v["id"].as_str() { self.aborted.insert(id.into()); }
            }
            Event::Other(v) if v["type"] == "tool_execution_start" => {
                let owners: Vec<&str> = match v["delivery_id"].as_str() {
                    Some(id) => vec![id],
                    // Steering can be merged into the existing tool cycle. Without
                    // a user marker every overlapping wake is a plausible owner.
                    None => self.active.iter().map(String::as_str).collect(),
                };
                let state = if v["delivery_id"].is_null() && owners.len() > 1 {
                    "effects_ambiguous"
                } else {
                    "effects_possible"
                };
                for id in owners {
                    self.db.execute("UPDATE deliveries SET state=CASE WHEN state='effects_ambiguous' THEN state ELSE ?2 END WHERE id=?1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous')", params![id, state])?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    /// Convenience for offline callers. Live hosts select on reset_deadline instead,
    /// then call flush_queued so observation collection is never held across a timer.
    pub async fn wait_for_reset(&mut self) -> Result<()> {
        let reset = self
            .reset
            .context("reset time absent; wait for a harness status update")?;
        tokio::time::sleep(std::time::Duration::from_secs(
            reset.saturating_sub(self.clock.now_ms() / 1000),
        ))
        .await;
        self.db
            .execute("DELETE FROM meta WHERE key='rate_reset'", [])?;
        self.reset = None;
        // Expiry restores inference availability, not certainty about tool effects.
        if matches!(
            self.status,
            Status::NeedsReconciliation(_) | Status::AuthBlocked(_)
        ) {
            return Ok(());
        }
        self.status = Status::Idle;
        self.flush_queued().await
    }
    pub fn engine_ready(&mut self) {
        if matches!(self.status, Status::WaitingEngine) {
            self.status = Status::Idle;
        }
    }
    pub fn inflight(&self) -> Result<Vec<String>> {
        let mut s=self.db.prepare("SELECT id FROM deliveries WHERE query=1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous','needs_reconciliation') ORDER BY rowid")?;
        let rows = s
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
    /// After a crash, reload or app restart: Pi's session file decides every interrupted delivery, so none waits
    /// for a click. A successful receipt completes it; any sign that a tool started (a failed receipt with effects,
    /// or the effects record written before each tool runs) settles it without replay. With no sign of a tool it is
    /// still never resent across a start (E1.14, E4.4): it ends, and its speech joins the start's catch-up as history.
    /// An interrupted context-only append is ended; its observation reaches the catch-up too.
    pub fn recover_receipts(&mut self, path: &Path) -> Result<()> {
        self.recover_from(crate::session::evidence(path)?)
    }
    /// `recover_receipts` with the session evidence already read (the simulator keeps Pi's session in memory).
    pub fn recover_from(&mut self, evidence: crate::session::Evidence) -> Result<()> {
        self.db.execute("UPDATE deliveries SET state='failed' WHERE query=0 AND state='needs_reconciliation'", [])?;
        for id in self.inflight()? {
            // Pi wrote the words down before the process went: the conversation holds them (the start's catch-up
            // skips them), whether or not this bridge heard Pi say so.
            if evidence.consumed.contains(&id) { self.mark_in_history(&id)?; }
            let receipt = evidence.receipts.iter().rev().find(|r| r.id == id);
            if receipt.is_some_and(|r| r.completed) {
                self.reconcile(&id, true)?;
            } else if receipt.is_some_and(|r| r.effects) || evidence.effects.contains(&id) {
                self.settle_after_error(&id, "interrupted after a tool started")?;
            } else {
                self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [&id])?;
                self.active.retain(|a| a != &id);
                self.resolutions.push(Resolution { id: id.clone(), outcome: Resolved::Failed, reason: "interrupted by a restart before any tool ran; its words are history".into() });
            }
        }
        self.refresh_unresolved()
    }
    pub fn reset_deadline(&self) -> Option<std::time::SystemTime> {
        self.reset
            .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s))
    }
    pub async fn flush_queued(&mut self) -> Result<()> {
        if let Some(epoch) = self.harness.delivery_epoch() {
            self.db.execute("UPDATE deliveries SET state='cancelled' WHERE state='queued' AND query=1 AND (json_type(wire,'$.delivery_epoch') IS NOT 'integer' OR json_extract(wire,'$.delivery_epoch')!=?1)", [epoch as i64])?;
        }
        // A turn queued while held (a wake in flight when the hold began, failed and queued for its resend) is held too:
        // no turn while the agent is down (review 27 Oct).
        if self.hold.is_some() { self.hold_queued().await?; }
        if self.blocked() {
            // Held input is context: it needs Pi, not the provider (emergency mode).
            if self.hold_appends() { return self.flush_notices().await; }
            return Ok(());
        }
        if matches!(self.status, Status::RateLimited { .. }) {
            self.db
                .execute("DELETE FROM meta WHERE key='rate_reset'", [])?;
            self.reset = None;
            self.status = Status::Idle;
        }
        loop {
            // Notices never steer: they go out below, once the turn has settled.
            let row=self.db.query_row("SELECT id,wire,query,last_seq FROM deliveries WHERE state='queued' AND json_extract(wire,'$.notice') IS NULL ORDER BY rowid LIMIT 1",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?,r.get::<_,i64>(3)?))).optional()?;
            let Some((id, wire, query, mut seq)) = row else {
                break;
            };
            if let Some(reason) = self.queue_reason() { self.record_queue(&id, &reason)?; break; }
            let mut wire: Value = serde_json::from_str(&wire)?;
            // A resend is only worth it while the failure is fresh (E1.14); later, its speech rides the next wake.
            if wire["auto_retry"] == true && wire["failed_at_ms"].as_u64().is_some_and(|at| self.clock.now_ms().saturating_sub(at) > notice::RESEND_WINDOW_MS) {
                self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [&id])?;
                self.resolutions.push(Resolution { id: id.clone(), outcome: Resolved::Failed, reason: "the resend waited longer than two minutes".into() });
                continue;
            }
            if wire["delivery_epoch"]
                .as_u64()
                .is_some_and(|epoch| self.harness.delivery_epoch() != Some(epoch))
            {
                self.db
                    .execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
                continue;
            }
            // A wake waiting out its retry delay holds the queue, unless the user has said or typed something since:
            // then it goes now, ahead of his newer words.
            let delayed = wire["retry_at_ms"].as_u64();
            if delayed.is_some_and(|at| self.clock.now_ms() < at) && !self.later_from_owner(&id)? { break; }
            if query {
                if let Some(instruction) = self
                    .db
                    .query_row(
                        "SELECT instruction FROM wake_instructions WHERE id=?1",
                        [&id],
                        |r| r.get::<_, String>(0),
                    )
                    .optional()?
                {
                    // Rebuilding an earlier wake must not pull later speech
                    // across a typed barrier, even though that speech is now
                    // present in the observation spool. Its own wake keeps it.
                    let typed_barrier: bool = self.db.query_row(
                        "SELECT EXISTS(SELECT 1 FROM deliveries d WHERE d.rowid>(SELECT rowid FROM deliveries WHERE id=?1) AND d.query=1 AND NOT EXISTS(SELECT 1 FROM wake_instructions w WHERE w.id=d.id))",
                        [&id], |r| r.get(0),
                    )?;
                    let (text, last_seq, ids) = self.build_transcript_through(&instruction, if typed_barrier { seq } else { wire["observation_ceiling"].as_i64().unwrap_or(i64::MAX) }, Some(&id))?;
                    wire.as_object_mut().unwrap().remove("transcript_context");
                    seq = last_seq;
                    wire["message"]["content"][0]["text"] = Value::String(text);
                    wire["observation_ids"] = serde_json::json!(ids);
                    self.db.execute(
                        "UPDATE deliveries SET wire=?2,last_seq=?3 WHERE id=?1",
                        params![id, wire.to_string(), seq],
                    )?;
                }
            }
            self.deliver(&id, wire, query, seq).await?;
            if self.is_queued(&id)? { break; }
            if delayed.is_some() { self.resolutions.push(Resolution { id: id.clone(), outcome: Resolved::Resent, reason: String::new() }); }
            if query {
                self.status = Status::Working;
            }
        }
        self.flush_notices().await
    }
    /// Reported outcomes of deliveries that resolved themselves since the last call.
    pub fn take_resolutions(&mut self) -> Vec<Resolution> {
        std::mem::take(&mut self.resolutions)
    }
    /// A delivery no tool touched: queue a resend (the transcript is marked as repeated), or end it once its resends
    /// failed too. Most failures get one resend at once. The user's wake that failed for a passing reason
    /// (`notice::transient_failure`) gets one after each of `notice::RETRY_DELAYS_MS` (bug notes 26 Oct 00:52:29,
    /// 00:54:39). Its observations stay unsettled either way, so the speech is never lost.
    fn retry(&mut self, id: &str, reason: &str) -> Result<()> {
        let raw: String = self.db.query_row("SELECT wire FROM deliveries WHERE id=?1", [id], |r| r.get(0))?;
        let mut wire: Value = serde_json::from_str(&raw)?;
        let done = wire["retries"].as_u64().unwrap_or(u64::from(wire["auto_retry"] == true)) as usize;
        let backoff = wire["notice"].is_null() && notice::transient_failure(reason);
        let limit = if backoff { notice::RETRY_DELAYS_MS.len() } else { 1 };
        let outcome = if done >= limit {
            self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [id])?;
            Resolved::Failed
        } else {
            let now = self.clock.now_ms();
            wire["auto_retry"] = serde_json::json!(true);
            wire["retries"] = serde_json::json!(done + 1);
            wire["failed_at_ms"] = serde_json::json!(now);
            wire["transcript_attempted"] = serde_json::json!(true);
            match backoff {
                true => wire["retry_at_ms"] = serde_json::json!(now + notice::RETRY_DELAYS_MS[done]),
                false => { wire.as_object_mut().unwrap().remove("retry_at_ms"); }
            }
            self.db.execute("UPDATE deliveries SET wire=?2,state='queued' WHERE id=?1", params![id, wire.to_string()])?;
            self.record_queue(id, "Retrying after an agent error")?;
            Resolved::Retrying
        };
        self.active.retain(|a| a != id);
        self.resolutions.push(Resolution { id: id.into(), outcome, reason: reason.into() });
        Ok(())
    }
    /// A delivery whose turn may have run a tool: completed as far as it got, its speech settled, never replayed.
    fn settle_after_error(&mut self, id: &str, reason: &str) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("UPDATE deliveries SET state='completed' WHERE id=?1", [id])?;
        let query: bool = tx.query_row("SELECT query FROM deliveries WHERE id=?1", [id], |r| r.get(0))?;
        if query { Self::settle_observations(&tx, id)?; }
        tx.commit()?;
        self.active.retain(|a| a != id);
        self.resolutions.push(Resolution { id: id.into(), outcome: Resolved::SettledAfterError, reason: reason.into() });
        Ok(())
    }
    /// Uncertain = marked for reconciliation, or in flight without a live turn that owns it (after a restart).
    fn refresh_unresolved(&mut self) -> Result<()> {
        let rows = self.db.prepare("SELECT id,state FROM deliveries WHERE (query=1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous')) OR state='needs_reconciliation' ORDER BY rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let live = !matches!(self.status, Status::NeedsReconciliation(_));
        let unresolved = rows.into_iter().find(|(id, state)| state == "needs_reconciliation" || !live || !self.active.contains(id)).map(|(id, _)| id);
        match unresolved {
            Some(id) => self.status = Status::NeedsReconciliation(id),
            None if matches!(self.status, Status::NeedsReconciliation(_)) => {
                self.status = if self.active.is_empty() { Status::Idle } else { Status::Working };
            }
            None => {}
        }
        Ok(())
    }
    fn settle_observations(db: &Connection, id: &str) -> Result<()> {
        let raw: String = db.query_row("SELECT wire FROM deliveries WHERE id=?1", [id], |r|r.get(0))?;
        for observation in protocol::observation_ids(&serde_json::from_str::<Value>(&raw)?) {
            db.execute("UPDATE observations SET settled=1 WHERE id=?1", [observation])?;
        }
        Ok(())
    }
    /// Caller reconciles effects after a crash. Never blindly replays tool-bearing wakes.
    pub fn reconcile(&mut self, id: &str, completed: bool) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute(
            "UPDATE deliveries SET state=?2 WHERE id=?1",
            params![id, if completed { "completed" } else { "failed" }],
        )?;
        if completed {
            let (query, raw): (bool, String) = tx.query_row("SELECT query,wire FROM deliveries WHERE id=?1", [id], |r|Ok((r.get(0)?,r.get(1)?)))?;
            if !query {
                for observation in protocol::observation_ids(&serde_json::from_str::<Value>(&raw)?) {
                    tx.execute("UPDATE observations SET supplied=1 WHERE id=?1", [observation])?;
                }
            }
            if query { Self::settle_observations(&tx, id)?; }
        }
        tx.commit()?;
        self.active.retain(|a| a != id);
        let unresolved = self.db.query_row("SELECT id FROM deliveries WHERE (query=1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous')) OR state='needs_reconciliation' ORDER BY rowid LIMIT 1", [], |r| r.get::<_, String>(0)).optional()?;
        self.status = unresolved
            .map(Status::NeedsReconciliation)
            .unwrap_or(Status::Idle);
        Ok(())
    }
    /// Oldest unresolved wakes first. SQL projects bounded text/references, never a whole base64 wire.
    /// The unresolved id list remains complete; callers fetch another batch after resolving these rows.
    pub fn delivery_reviews(&self) -> Result<Vec<DeliveryReview>> {
        let mut stmt = self.db.prepare("SELECT id,substr(json_extract(wire,'$.message.content[0].text'),1,?1),json_extract(wire,'$.timestamp_ms') FROM deliveries WHERE (query=1 AND state IN ('submitted','echoed','effects_possible','effects_ambiguous')) OR state='needs_reconciliation' ORDER BY rowid LIMIT ?2")?;
        let rows = stmt.query_map(params![(REVIEW_TEXT_BYTES + 1) as i64, REVIEW_ROWS as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get::<_, Option<i64>>(2)?.and_then(|n| u64::try_from(n).ok())))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut reviews = Vec::new();
        for (id, mut text, timestamp_ms) in rows {
            let text_truncated = text.len() > REVIEW_TEXT_BYTES;
            if text_truncated {
                let mut end = REVIEW_TEXT_BYTES;
                while !text.is_char_boundary(end) { end -= 1; }
                text.truncate(end);
            }
            let mut images = self.db.prepare("SELECT json_extract(b.value,'$.source.media_type'),json_extract(b.value,'$.source.archive_image_id'),CASE WHEN json_extract(b.value,'$.source.archive_image_id') IS NULL THEN json_extract(b.value,'$.source.data') END FROM deliveries d,json_each(d.wire,'$.message.content') b WHERE d.id=?1 AND json_extract(b.value,'$.type')='image' LIMIT ?2")?;
            let refs = images.query_map(params![id, (REVIEW_ATTACHMENTS + 1) as i64], |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(),r.get::<_, Option<String>>(1)?,r.get::<_, Option<String>>(2)?)))?;
            let mut attachments_truncated = false;
            let mut attachments = Vec::new();
            for (index, row) in refs.enumerate() {
                if index == REVIEW_ATTACHMENTS { attachments_truncated = true; break; }
                let (mime, reference, data) = row?;
                let reference = match (reference, data) {
                    (Some(id), _) => Some(id),
                    (None, Some(data)) => {
                        use base64::Engine;
                        use sha2::Digest;
                        let bytes = base64::engine::general_purpose::STANDARD.decode(data)?;
                        Some(format!("{:x}", sha2::Sha256::digest(bytes)))
                    }
                    _ => None,
                };
                attachments.push((mime, reference));
            }
            reviews.push(DeliveryReview { id, text, timestamp_ms, attachments, text_truncated, attachments_truncated });
        }
        Ok(reviews)
    }
    pub fn queued_count(&self) -> Result<u64> {
        Ok(self.db.query_row(
            "SELECT count(*) FROM deliveries WHERE state='queued' AND query=1",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }
    pub fn last_queued(&self) -> Result<Option<Value>> {
        let wire:Option<String>=self.db.query_row("SELECT wire FROM deliveries WHERE state='queued' AND query=1 ORDER BY rowid DESC LIMIT 1",[],|r|r.get(0)).optional()?;
        wire.map(|s| serde_json::from_str(&s).map_err(anyhow::Error::from))
            .transpose()
    }
    /// Whether this spool has the observation: speech heard while the agent was on (or already handed over).
    pub fn has_observation(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row("SELECT EXISTS(SELECT 1 FROM observations WHERE id=?1)", [id], |r| r.get(0))?)
    }
    /// Records speech the conversation received as added context: supplied and settled, so no wake carries it again
    /// and a later catch-up skips it.
    pub fn record_supplied(&mut self, rows: &[(String, String)]) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for (id, text) in rows {
            tx.execute("INSERT INTO observations(id,text,supplied,settled) VALUES(?1,?2,1,1) ON CONFLICT(id) DO UPDATE SET supplied=1,settled=1,dropped=NULL", params![id, text])?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn pending_observations(&self) -> Result<u64> {
        Ok(self.db.query_row("SELECT count(*) FROM observations WHERE settled=0",[],|r|r.get::<_,i64>(0))? as u64)
    }
    /// Archive committed terminal payloads before replacing their hot bodies.
    pub fn retire(&mut self, archive:&PayloadArchive)->Result<()> {
        let rows=self.db.prepare("SELECT id,wire FROM deliveries WHERE state IN ('completed','failed','cancelled','coalesced') AND json_extract(wire,'$.archive_ref') IS NULL LIMIT 100")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id,wire) in rows {
            let instruction:Option<String>=self.db.query_row("SELECT instruction FROM wake_instructions WHERE id=?",[&id],|r|r.get(0)).optional()?;
            let reference=archive(&serde_json::json!({"store":"delivery","id":id,"wire":serde_json::from_str::<Value>(&wire)?,"instruction":instruction}))?;
            let tx=self.db.transaction()?;
            tx.execute("UPDATE deliveries SET wire=? WHERE id=?",params![serde_json::json!({"uuid":id,"archive_ref":reference}).to_string(),id])?;
            tx.execute("DELETE FROM wake_instructions WHERE id=?",[&id])?;tx.commit()?;
        }
        let rows=self.db.prepare("SELECT seq,id,text FROM observations WHERE text NOT LIKE 'archive:%' AND settled=1 LIMIT 100")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        // Keep ids for observation deduplication; only the body is retired.
        for (seq,id,text) in rows {if !text.starts_with("archive:"){let reference=archive(&serde_json::json!({"store":"observation","id":id,"text":text}))?; self.db.execute("UPDATE observations SET text=? WHERE seq=?",params![format!("archive:{reference}"),seq])?;}}
        Ok(())
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.harness.shutdown().await?;
        self.status = Status::Stopped;
        Ok(())
    }
    pub fn clock(&self) -> &clock::ClockHandle { &self.clock }
    /// After a start and before the user's first addressed input (E1.16).
    pub fn awaiting_owner(&self) -> bool { self.awaiting_owner }
    /// The start's catch-up and restart message are not in the conversation yet; every wake waits for them.
    pub fn start_pending(&self) -> bool { self.start_pending }
    /// A turn is running or a delivery still waits on its outcome (a wake restarted by the user's words is running again
    /// when the aborted turn's completions arrive).
    pub fn turn_running(&self) -> bool { !self.active.is_empty() || self.harness.turn_active() }
    const UNRESOLVED_OWNER: &'static str = "EXISTS(SELECT 1 FROM deliveries d,json_each(d.wire,'$.observation_ids') j WHERE d.query=1 AND j.value=o.id
        AND d.state IN ('queued','submitted','echoed','effects_possible','effects_ambiguous','needs_reconciliation'))";
    /// Pending not-for-Heimdall lines older than the context window stay in the archive (the timeline search finds
    /// them) instead of riding a wake hours later (root cause 4).
    fn drop_stale_context(&mut self) -> Result<u64> {
        let cutoff = self.clock.now_ms().saturating_sub(notice::CONTEXT_WINDOW_MS) as i64;
        let sql = format!("UPDATE observations AS o SET settled=1,dropped='older than 30 minutes when a wake went out'
            WHERE settled=0 AND at_ms IS NOT NULL AND at_ms<?1 AND NOT {}", Self::UNRESOLVED_OWNER);
        Ok(self.db.execute(&sql, [cutoff])? as u64)
    }
    /// A non-the user message to the agent (E1.6–E1.10): durable and deduplicated by its deterministic id (E6.3), and by
    /// its words within 24 hours (E6.4). It never steers a turn: it waits for the turn to settle (E2.2), coalesces with
    /// every other waiting notice into one message, answers first (E2.3), and at most one notice-only turn runs per
    /// 30 s (E1.18). Context kinds are appended without a turn. After a start, until the user addresses Heimdall, worker
    /// events are context and questions wait (E4.3). `key` is the delivery id (derived from the notice's identity),
    /// `text` its own words, which the model sees under `[Heimdall notice: <label>]`. False when it was a duplicate.
    pub async fn notice(&mut self, key: &str, label: &str, text: &str, kind: NoticeKind, permit: Option<delivery::DeliveryPermit>) -> Result<bool> {
        if let Some(p) = &permit { p.check()?; }
        if text.len() > self.harness.max_text_bytes() { return Err(MessageRejected.into()); }
        let known = self.db.query_row("SELECT 1 FROM deliveries WHERE id=?1 UNION ALL SELECT 1 FROM wake_keys WHERE id=?1", [key], |r| r.get::<_, i64>(0)).optional()?.is_some();
        if known { return Ok(false); }
        let now = self.clock.now_ms();
        let digest = body_digest(text);
        let epoch = permit.as_ref().map(|p| p.generation()).or_else(|| self.harness.delivery_epoch());
        // Questions and approvals are judged by their id alone: an approval's words carry no run or request id, so the
        // same words on another run are a new request (E1.8). Other notices repeat-check their words (E6.4).
        let repeated = !matches!(kind, NoticeKind::Answer | NoticeKind::Question) && self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM deliveries WHERE json_extract(wire,'$.notice_digest')=?1 AND json_extract(wire,'$.timestamp_ms')>=?2 AND state!='cancelled')",
            params![digest, now.saturating_sub(notice::REPEAT_WINDOW_MS) as i64], |r| r.get::<_, bool>(0))?;
        // A run's result withdraws its approval notice still waiting (after a start, questions wait for the user): the run
        // is over, so nothing waits for him there (bug note 2026-10-28T02-20-08, delivered 11 minutes after the run).
        // `terminal:{thread}:{run}:{state}`, or `…:failed:auth` for a run that ended on its provider's sign-in.
        if let Some(run) = label.strip_prefix("terminal:").map(|r| r.strip_suffix(":auth").unwrap_or(r)).and_then(|r| r.rsplit_once(':')).map(|(run, _)| run) {
            let stale = format!("[Heimdall notice: approval:{run}]\n");
            let n = self.db.execute("UPDATE deliveries SET state='cancelled' WHERE state='queued' AND substr(json_extract(wire,'$.message.content[0].text'),1,length(?1))=?1", [&stale])?;
            if n > 0 { eprintln!("Notice approval:{run} withdrawn: its run ended before it was delivered"); }
        }
        // The user's click on Heimdall's question is his input: an idle compaction gives way to it (night review 9).
        if kind == NoticeKind::Answer { self.awaiting_owner = false; self.owner_input().await?; }
        let kind = if self.awaiting_owner && kind == NoticeKind::Event { NoticeKind::Context } else { kind };
        if self.hold.is_some() && kind.wakes() && !repeated && !label.starts_with(notice::EMERGENCY_LATE) {
            self.hold_input(key, kind.as_str(), &format!("[Heimdall notice: {label}]\n{text}"), &[], &[], 0, epoch).await?;
            return Ok(true);
        }
        let mut wire = message(key, &format!("[Heimdall notice: {label}]\n{text}"), &[], kind.wakes(), false)?;
        wire["timestamp_ms"] = serde_json::json!(now);
        wire["notice"] = serde_json::json!(kind.as_str());
        wire["notice_digest"] = serde_json::json!(digest);
        if let Some(epoch) = epoch { wire["delivery_epoch"] = serde_json::json!(epoch); }
        if repeated { wire["queue_reason"] = serde_json::json!("Same words already delivered within 24 hours"); }
        // The id stays known either way, so a repeated callback is still deduplicated.
        self.db.execute("INSERT INTO deliveries VALUES(?1,?2,?3,0,?4)", params![key, wire.to_string(), kind.wakes(), if repeated { "cancelled" } else { "queued" }])?;
        if repeated { return Ok(false); }
        self.flush_queued().await?;
        Ok(true)
    }
    /// Waiting notices go out once the turn has settled: context kinds as appends, the rest as one coalesced
    /// notice-only turn, at most one per `NOTICE_GAP_MS`.
    async fn flush_notices(&mut self) -> Result<()> {
        let held_only = self.blocked() && self.hold_appends();
        if (self.blocked() && !held_only) || self.start_pending || self.harness.input_blocked().is_some() { return Ok(()); }
        if self.turn_running() {
            // The user's click on Heimdall's own question steers the running turn, in the order he clicked (card answers "should not be queued, they should be steering messages"). Other notices wait.
            if self.harness.supports_steering() { self.steer_answers().await?; }
            return Ok(());
        }
        let now = self.clock.now_ms();
        let epoch = self.harness.delivery_epoch();
        let gap_open = self.last_notice_turn_ms.is_some_and(|last| now < last + notice::NOTICE_GAP_MS);
        let rows = self.db.prepare("SELECT id,wire FROM deliveries WHERE state='queued' AND json_extract(wire,'$.notice') IS NOT NULL ORDER BY rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut wakes: Vec<(NoticeKind, String, Value)> = Vec::new();
        for (id, raw) in rows {
            let mut wire: Value = serde_json::from_str(&raw)?;
            if wire["delivery_epoch"].as_u64().is_some_and(|e| Some(e) != epoch) {
                self.db.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
                continue;
            }
            let mut kind = notice::stored_kind(&wire).unwrap_or(NoticeKind::Event);
            // A notice that waited longer than the window is history now, not a reason for a turn (E2.4). A task's
            // result is still the answer the agent waits for (pi-exit, 21 Oct).
            if kind.wakes() && !matches!(kind, NoticeKind::Answer | NoticeKind::Scheduled | NoticeKind::Result) && now.saturating_sub(wire["timestamp_ms"].as_u64().unwrap_or(now)) > notice::CONTEXT_WINDOW_MS {
                kind = NoticeKind::Context;
                wire["notice"] = serde_json::json!(kind.as_str());
                wire["shouldQuery"] = serde_json::json!(false);
                self.db.execute("UPDATE deliveries SET wire=?2,query=0 WHERE id=?1", params![id, wire.to_string()])?;
            }
            if kind == NoticeKind::Context {
                let text = wire["message"]["content"][0]["text"].as_str().unwrap_or("").to_owned();
                match self.harness.append_context(&text).await {
                    Ok(()) => { self.db.execute("UPDATE deliveries SET state='appended' WHERE id=?1", [&id])?; }
                    Err(e) if e.downcast_ref::<harness::InputUnavailable>().is_some() || e.downcast_ref::<harness::EngineUnavailable>().is_some() => return Ok(()),
                    Err(e) if e.downcast_ref::<delivery::DeliveryBlocked>().is_some() => {
                        self.db.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
                    }
                    Err(e) => {
                        // Context that cannot be added is not a request; it stays in the delivery store, never a loop.
                        self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [&id])?;
                        self.resolutions.push(Resolution { id: id.clone(), outcome: Resolved::Failed, reason: format!("context not added: {e:#}") });
                    }
                }
                continue;
            }
            if kind == NoticeKind::Question && self.awaiting_owner { continue; }
            if held_only { continue; }
            wakes.push((kind, id, wire));
        }
        if wakes.is_empty() { return Ok(()); }
        // An answer to Heimdall's own question is the user's click: it never waits behind the notice gap (E11.3).
        let answered = wakes.iter().any(|(kind, _, _)| *kind == NoticeKind::Answer);
        if gap_open && !answered { return Ok(()); }
        wakes.sort_by_key(|(kind, _, _)| kind.rank());
        let (_, carrier, mut wire) = wakes[0].clone();
        if wakes.len() > 1 {
            // The carrier takes the strongest kind it carries: a scheduled wake or a task's result survives a start
            // that finds the carrier still queued (review 21 Oct: merged into an older update's delivery, it became
            // history with it).
            for strong in [NoticeKind::Scheduled, NoticeKind::Result] {
                if wakes.iter().any(|(kind, _, _)| *kind == strong) {
                    wire["notice"] = serde_json::json!(strong.as_str());
                    break;
                }
            }
            let text = wakes.iter().filter_map(|(_, _, w)| w["message"]["content"][0]["text"].as_str()).collect::<Vec<_>>().join("\n\n");
            wire["message"]["content"][0]["text"] = Value::String(text);
            wire["coalesced_notices"] = serde_json::json!(wakes.iter().map(|(_, id, _)| id).collect::<Vec<_>>());
            let tx = self.db.unchecked_transaction()?;
            tx.execute("UPDATE deliveries SET wire=?2 WHERE id=?1", params![carrier, wire.to_string()])?;
            for (_, other, _) in &wakes[1..] {
                tx.execute("UPDATE deliveries SET state='coalesced' WHERE id=?1", [other])?;
                tx.execute("INSERT OR IGNORE INTO wake_keys VALUES(?1,?2)", params![other, carrier])?;
            }
            tx.commit()?;
        }
        self.deliver(&carrier, wire, true, 0).await?;
        if !self.is_queued(&carrier)? {
            self.last_notice_turn_ms = Some(now);
            self.status = Status::Working;
        }
        Ok(())
    }
    /// Queued answer clicks go into the running turn as steers, one each, oldest first.
    async fn steer_answers(&mut self) -> Result<()> {
        let epoch = self.harness.delivery_epoch();
        let rows = self.db.prepare("SELECT id,wire FROM deliveries WHERE state='queued' AND json_extract(wire,'$.notice')='answer' ORDER BY rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, raw) in rows {
            let wire: Value = serde_json::from_str(&raw)?;
            if wire["delivery_epoch"].as_u64().is_some_and(|e| Some(e) != epoch) {
                self.db.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
                continue;
            }
            self.deliver(&id, wire, true, 0).await?;
            if self.is_queued(&id)? { break; }
        }
        Ok(())
    }
    /// When a waiting notice may go out (the notice gap ends), for the host's timer.
    pub fn notice_deadline(&self) -> Option<u64> {
        let due = self.last_notice_turn_ms? + notice::NOTICE_GAP_MS;
        // Answers never wait for the gap (flush_notices); every other waking kind may (a scheduled wake or a result
        // behind another notice's turn, pi-exit 21 Oct).
        let waiting = self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state='queued' AND json_extract(wire,'$.notice') IN ('question','event','result','scheduled'))", [], |r| r.get::<_, bool>(0)).unwrap_or(false);
        (waiting && self.clock.now_ms() < due).then_some(due)
    }
    /// A fresh agent process starts serving: app launch with the agent on, Agent on, a model switch, a reload, crash
    /// or supervisor recovery (E4). Everything pending from before is history (E4.4): queued typed text and notices
    /// (except open questions, which wait for the user) and unsettled spool rows heard within the window become catch-up
    /// lines; older rows stay archive-only; nothing is resent. A spool row never reached the model, so it comes back
    /// whatever the conversation's end or its last compaction (night review 21 Oct: a reload after an idle compaction,
    /// or Agent off/on after a failed request, dropped speech the agent never received); only a row whose words Pi
    /// already took in (`in_history`: a request that then failed) stays out. One exception: the user's own wake or typed
    /// text queued within `START_RESUME_MS` of the start (it waited for a reload, Pi's compaction or the restart and
    /// never reached the model) stays queued, with its speech, and goes out after the start context (bug note 20 Oct
    /// 22:12:57). Until the start context is in the conversation every wake waits (E4.1); until the user addresses
    /// Heimdall no notice wakes it (E4.3).
    pub fn begin_start(&mut self, reason: Option<String>) -> Result<()> {
        let now = self.clock.now_ms();
        let cutoff = now.saturating_sub(notice::CONTEXT_WINDOW_MS);
        // Where the archive's part of the catch-up begins (`catchup_since`); the header names the earliest line.
        let since = self.catchup_since(now)?;
        let mut lines = Vec::new();
        let mut claims = Vec::new();
        let mut resumed = false;
        let epoch = self.harness.delivery_epoch();
        let tx = self.db.unchecked_transaction()?;
        {
            let queued = tx.prepare("SELECT d.id,d.wire,EXISTS(SELECT 1 FROM wake_instructions w WHERE w.id=d.id) FROM deliveries d WHERE d.state='queued' ORDER BY d.rowid")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, raw, voice) in queued {
                let wire: Value = serde_json::from_str(&raw)?;
                let kind = notice::stored_kind(&wire);
                // An open question that never reached the model waits for the user; one that was sent and is waiting for
                // its resend is history like everything else (never resent across a start, E1.14). A scheduled wake
                // that never reached the model likewise stays a wake (agent::wakes: it was asked for this moment), and
                // so does a task's result (pi-exit, 21 Oct: none lost across a restart).
                // Scheduled wakes and results: only a request that failed at the provider (`auto_retry`: its words may
                // be in the conversation) is history; one Pi could not take (nothing written) still goes out.
                let unsent = match kind {
                    Some(NoticeKind::Question) => wire["transcript_attempted"] != true,
                    Some(NoticeKind::Scheduled | NoticeKind::Result) => wire["auto_retry"] != true,
                    _ => false,
                };
                if unsent {
                    if kind == Some(NoticeKind::Result) && wire["notice"] != "result" {
                        let mut wire = wire.clone();
                        wire["notice"] = serde_json::json!("result");
                        tx.execute("UPDATE deliveries SET wire=?2 WHERE id=?1", params![id, wire.to_string()])?;
                    }
                    continue;
                }
                // The user's own request that queued moments ago because the agent could not take it then (a reload
                // waiting for its turn, Pi compacting, a restart) never reached the model: it stays queued and goes
                // out after the start context, so he gets his answer (bug note 20 Oct 22:12:57). Not a resend of a
                // failed delivery (E1.14), not someone else's line, not from before an Off/On (another epoch).
                let fresh = wire["timestamp_ms"].as_u64().is_some_and(|at| at <= now && now - at <= notice::START_RESUME_MS);
                let same_epoch = epoch.is_none_or(|e| wire["delivery_epoch"].as_u64() == Some(e));
                if kind.is_none() && fresh && same_epoch && wire["from_owner"] != false && wire["auto_retry"] != true {
                    resumed = true;
                    continue;
                }
                if !voice {
                    let text = wire["message"]["content"][0]["text"].as_str().unwrap_or("").trim().to_owned();
                    // A legacy row with no time is history of unknown age: it reads `(earlier)`, not `now`.
                    let at = wire["timestamp_ms"].as_u64();
                    if at.is_none_or(|at| at >= cutoff) && !text.is_empty() {
                        let text = match (kind, text.strip_prefix("[Heimdall typed message]\n")) {
                            (Some(_), _) => format!("[Notice] {text}"),
                            (None, Some(typed)) => format!("[Typed · User] {typed}"),
                            (None, None) => format!("[Message] {text}"),
                        };
                        lines.push(notice::CatchupLine { id: id.clone(), at_ms: at, text, observation: false });
                    }
                }
                tx.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
            }
            let pending = tx.prepare(&format!("SELECT id,text,at_ms,in_history FROM observations o WHERE settled=0 AND NOT {} ORDER BY seq", Self::UNRESOLVED_OWNER))?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, bool>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, text, at, in_history) in pending {
                // Settled only once the start context is appended (`deliver_start_context`): a start that fails keeps
                // them for the next start instead of losing them (E4.4).
                claims.push(id.clone());
                match at.map(|a| a as u64) {
                    _ if in_history => { tx.execute("UPDATE observations SET dropped='pending before a start, already in the conversation (a request that failed)' WHERE id=?1", [&id])?; }
                    Some(at) if at >= cutoff && !text.starts_with("archive:") => lines.push(notice::CatchupLine { id, at_ms: Some(at), text: protocol::compact_transcript(&text), observation: true }),
                    _ => { tx.execute("UPDATE observations SET dropped='pending before a start, older than 30 minutes' WHERE id=?1", [&id])?; }
                }
            }
        }
        tx.commit()?;
        self.start_leftovers = lines;
        self.start_reason = reason;
        self.start_since = Some(since);
        self.start_claims = claims.into_iter().collect();
        self.start_pending = true;
        // A request of his still waiting to go out means he has addressed Heimdall (E4.3).
        self.awaiting_owner = !resumed;
        self.last_notice_turn_ms = None;
        // Agent on usually means the user is about to talk: a cold-cache compaction waits for quiet like any other, so
        // his first words go out at once (19 Oct 18:12: they waited 45 s for one).
        self.last_activity_ms = self.clock.now_ms();
        self.maintenance_checked = None;
        Ok(())
    }
    /// The history `begin_start` collected (for a host that builds its own catch-up).
    pub fn start_leftovers(&self) -> &[notice::CatchupLine] { &self.start_leftovers }
    /// Where the archived part of a start's catch-up (speech the spool never held, `start_context` in the app) begins
    /// (E4.1, night pass 20→21 Oct): after what the conversation already holds — its newest message (a failed
    /// request's words included), or the start of the last compaction that finished, whichever is later — and never
    /// more than 30 minutes back. So a start right after a compaction carries only archived speech from after the
    /// compaction began, and a plain restart only what came after the conversation's last message. Spool rows that
    /// never reached the model do not depend on it (`begin_start`).
    pub fn catchup_since(&self, on_ms: u64) -> Result<u64> {
        let floor = on_ms.saturating_sub(notice::CONTEXT_WINDOW_MS);
        Ok([Some(floor), self.harness.history_end_ms(), self.meta_u64(HISTORY_COMPACTED_FROM)?].into_iter().flatten().max().unwrap_or(floor))
    }
    /// The delivery's words are in the conversation (Pi read them): its observations never return in a catch-up.
    fn mark_in_history(&self, delivery: &str) -> Result<()> {
        self.db.execute("UPDATE observations SET in_history=1 WHERE id IN (SELECT j.value FROM deliveries d,json_each(d.wire,'$.observation_ids') j WHERE d.id=?1)", [delivery])?;
        Ok(())
    }
    fn meta_u64(&self, key: &str) -> Result<Option<u64>> {
        Ok(self.db.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok()))
    }
    fn set_meta(&self, key: &str, value: u64) -> Result<()> {
        self.db.execute("INSERT INTO meta VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value.to_string()])?;
        Ok(())
    }
    /// Appends the start context (catch-up lines, reason, restart message) once the new process is idle, compacting
    /// first when the catch-up would pass `threshold(window)` (E4.2). True once added; false while waiting for a turn
    /// or the compaction it started. On an error the host retries once on the next start or calls
    /// `abandon_start_context`.
    pub async fn deliver_start_context(&mut self, ctx: &mut notice::StartContext, threshold: impl Fn(u64) -> u64) -> Result<bool> {
        if self.turn_running() || self.harness.input_blocked().is_some() { return Ok(false); }
        // Kept until the context is appended: a failed attempt leaves them for the retry.
        ctx.add_lines(self.start_leftovers.iter().cloned());
        if ctx.reason.is_none() { ctx.reason = self.start_reason.clone(); }
        if ctx.since_ms.is_none() { ctx.since_ms = self.start_since; }
        let text = ctx.text();
        // The restart message alone is a few dozen tokens: only a catch-up can pass the threshold.
        if !ctx.compacted && !ctx.lines.is_empty() {
            if let Some((Some(tokens), window)) = self.harness.context_tokens().await? {
                // About four characters per token.
                let added = text.len() as u64 / 4;
                if tokens + added > threshold(window) {
                    ctx.compacted = true;
                    self.harness.compact_history().await?;
                    return Ok(false);
                }
            }
        }
        self.harness.append_context(&text).await?;
        let supplied: Vec<(String, String)> = ctx.lines.iter().filter(|l| l.observation).map(|l| (l.id.clone(), l.text.clone())).collect();
        self.record_supplied(&supplied)?;
        self.settle_start_claims()?;
        self.start_leftovers.clear();
        self.start_reason = None;
        self.start_pending = false;
        self.flush_queued().await?;
        Ok(true)
    }
    /// Settles the spool observations the start context carried, now that it is in the conversation (E4.4).
    fn settle_start_claims(&mut self) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        for id in &self.start_claims { tx.execute("UPDATE observations SET settled=1 WHERE id=?1", [id])?; }
        tx.commit()?;
        self.start_claims.clear();
        Ok(())
    }
    /// The start context could not be added: wakes stop waiting for it. Its observations stay unsettled, so the next
    /// start carries them again, and no wake carries them meanwhile.
    pub fn abandon_start_context(&mut self) { self.start_pending = false; self.start_leftovers.clear(); self.start_reason = None; }
    /// The user's Stop (E3), before the turn is aborted. Waiting notices become context, appended without a turn (E3.4).
    /// Voice wakes queued before Stop are not delivered; their speech stays pending, context for the user's next wake
    /// (E3.6, S27). Typed text queued before Stop is added as context. Nothing starts a turn until the user speaks again.
    pub fn stop(&mut self) -> Result<()> {
        let tx = self.db.unchecked_transaction()?;
        {
            let queued = tx.prepare("SELECT d.id,d.wire,EXISTS(SELECT 1 FROM wake_instructions w WHERE w.id=d.id) FROM deliveries d WHERE d.state='queued' ORDER BY d.rowid")?
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (id, raw, voice) in queued {
                let mut wire: Value = serde_json::from_str(&raw)?;
                if voice {
                    tx.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
                    continue;
                }
                if wire["notice"].is_null() {
                    let text = wire["message"]["content"][0]["text"].as_str().unwrap_or("").to_owned();
                    wire["message"]["content"][0]["text"] = Value::String(format!("(Sent before User pressed Stop; not a request now.)\n{text}"));
                }
                wire["notice"] = serde_json::json!(NoticeKind::Context.as_str());
                wire["shouldQuery"] = serde_json::json!(false);
                tx.execute("UPDATE deliveries SET wire=?2,query=0 WHERE id=?1", params![id, wire.to_string()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    /// Emergency mode on (`Some`) or off (docs/emergency.md; ~00:05: "all of the things that would normally
    /// go to the context and to the Pi agent, they will actually go to it, but they won't wake up the agent. They will
    /// be relayed to the T3 Code instance"). While on, each voice wake, typed message and waking notice is added to the
    /// conversation as context marked `emergency::HELD` and handed to `relay`; nothing starts a turn. Turning it on
    /// holds what is already queued the same way: the user's queued voice wakes and the words the failed requests left
    /// unanswered become one held wake. Off: input takes the normal path again.
    pub async fn hold_turns(&mut self, relay: Option<emergency::Relay>) -> Result<()> {
        let entering = self.hold.is_none() && relay.is_some();
        self.hold = relay;
        if !entering { return Ok(()); }
        // The user's queued voice wakes and typed text, then the waking notices.
        self.hold_queued().await?;
        let queued = self.db.prepare("SELECT id,wire FROM deliveries WHERE state='queued' AND json_extract(wire,'$.notice') IS NOT NULL AND json_extract(wire,'$.held') IS NULL ORDER BY rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, raw) in queued {
            let wire: Value = serde_json::from_str(&raw)?;
            let Some(kind) = notice::stored_kind(&wire).filter(|k| k.wakes()) else { continue };
            self.db.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
            // A hand-back's "while you were down" a restart left queued is Heimdall's own: a later probe writes a new one.
            if id.starts_with(notice::EMERGENCY_BACK) || id.starts_with(notice::EMERGENCY_BACK_OLD) { continue; }
            let text = wire["message"]["content"][0]["text"].as_str().unwrap_or("").to_owned();
            self.hold_input(&format!("{id}:held"), kind.as_str(), &text, &[], &[], 0, wire["delivery_epoch"].as_u64()).await?;
        }
        // What the failed requests left unanswered, every speaker labelled.
        self.drop_stale_context()?;
        let (text, last_seq, ids) = self.build_transcript_through("Heard and never answered by Heimdall's failing agent (each line names its speaker):", i64::MAX, None)?;
        if !ids.is_empty() {
            let epoch = self.harness.delivery_epoch();
            self.hold_input(&uuid::Uuid::new_v4().to_string(), "wake", &text, &[], &ids, last_seq, epoch).await?;
        }
        Ok(())
    }
    /// Every queued turn (voice wake, typed text) becomes held input; a cache-refresh ping is dropped.
    async fn hold_queued(&mut self) -> Result<()> {
        let queued = self.db.prepare("SELECT d.id,d.wire,d.last_seq,EXISTS(SELECT 1 FROM wake_instructions w WHERE w.id=d.id) FROM deliveries d WHERE d.state='queued' AND d.query=1 AND json_extract(d.wire,'$.notice') IS NULL ORDER BY d.rowid")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, bool>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, raw, seq, voice) in queued {
            let wire: Value = serde_json::from_str(&raw)?;
            self.db.execute("UPDATE deliveries SET state='cancelled' WHERE id=?1", [&id])?;
            if is_cache_ping(&wire) { continue; }
            let text = wire["message"]["content"][0]["text"].as_str().unwrap_or("").to_owned();
            self.hold_input(&format!("{id}:held"), if voice { "wake" } else { "typed" }, &text, &[], &protocol::observation_ids(&wire), seq, wire["delivery_epoch"].as_u64()).await?;
        }
        Ok(())
    }
    pub fn holding_turns(&self) -> bool { self.hold.is_some() }
    /// Relay mode (`true`): idle compaction runs while turns are held (`hold_compacts`).
    pub fn set_hold_compacts(&mut self, on: bool) { self.hold_compacts = on; }
    /// One held input: context in the conversation (appended as soon as Pi is idle; a start's catch-up carries it if Pi
    /// is down), its speech settled with it, and the relay told.
    #[allow(clippy::too_many_arguments)]
    async fn hold_input(&mut self, id: &str, kind: &str, text: &str, images: &[Attachment], observations: &[String], last_seq: i64, epoch: Option<u64>) -> Result<()> {
        let Some(relay) = self.hold.clone() else { return Ok(()); };
        let text = if observations.is_empty() { text.to_owned() } else { protocol::compact_transcript(text) };
        let shots = if images.is_empty() { String::new() } else { format!("\n[{} image(s), shown to the cover agent]", images.len()) };
        let mut wire = message(id, &format!("{}\n{text}{shots}", emergency::HELD), &[], false, false)?;
        wire["timestamp_ms"] = serde_json::json!(self.clock.now_ms());
        wire["notice"] = serde_json::json!(NoticeKind::Context.as_str());
        wire["held"] = serde_json::json!(kind);
        wire["held_observations"] = serde_json::json!(observations);
        if let Some(epoch) = epoch { wire["delivery_epoch"] = serde_json::json!(epoch); }
        let tx = self.db.unchecked_transaction()?;
        tx.execute("INSERT OR IGNORE INTO deliveries VALUES(?1,?2,0,?3,'queued')", params![id, wire.to_string(), last_seq])?;
        for o in observations { tx.execute("UPDATE observations SET supplied=1,settled=1 WHERE id=?1", [o])?; }
        tx.commit()?;
        relay(emergency::Relayed { id: id.into(), kind: kind.into(), text, images: images.to_vec() });
        self.flush_notices().await
    }
    /// Held context may be appended while the provider side blocks turns (a rate limit, a failed sign-in): it needs Pi
    /// only. Not during a reload or an uncertain delivery.
    fn hold_appends(&self) -> bool {
        self.hold.is_some() && !matches!(self.status, Status::NeedsReconciliation(_) | Status::ReloadPending)
    }
    /// Work waits for an answer from the agent: a turn runs, or the user's voice or typed words are queued for one.
    /// Notices are not counted: a question waits for the user on purpose (`emergency::Detector::tick`).
    pub fn awaiting_answer(&self) -> bool {
        self.turn_running() || self.db.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE state='queued' AND query=1 AND json_extract(wire,'$.notice') IS NULL AND json_extract(wire,'$.cache_ping') IS NOT 1)", [], |r| r.get::<_, bool>(0)).unwrap_or(false)
    }
    /// The provider refused the request as invalid: never resent (the same request fails again). Its observations
    /// stay pending and ride the next wake once; its images do not (E10.2).
    fn reject(&mut self, id: &str, error: &str) -> Result<()> {
        self.db.execute("UPDATE deliveries SET state='failed' WHERE id=?1", [id])?;
        self.active.retain(|a| a != id);
        self.resolutions.push(Resolution { id: id.into(), outcome: Resolved::Rejected, reason: notice::provider_rejection(error, self.run_repaired).unwrap_or_default() });
        Ok(())
    }
}
impl Bridge<harness::DryRun> {
    pub fn dry_run(path: impl AsRef<Path>) -> Result<Self> {
        Self::open(harness::DryRun::default(), path)
    }
}

/// Early main-executable dispatch for the signed process guardian.
pub use process_owner::guardian_mode;

pub use process_owner::spawn_relaunch;
