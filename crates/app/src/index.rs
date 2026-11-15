use crate::archive::DiskGuard;
use anyhow::{ensure, Result};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// The session's final transcript projection (`utterances` in the archive store) and its journal.
/// Both hand their writes to the store's writer (heimdall_archive::live): a write waits for its commit at most
/// `live::COMMIT_WAIT` while the writer keeps up, and none is dropped. Every write job reads what it depends on inside
/// its own transaction, so a queued write still sees every earlier one.
pub struct Index {
    db: Connection,
    guard: DiskGuard,
    session: i64,
    name: String,
    root: PathBuf,
    queue: heimdall_archive::live::Queue,
    pub evidence: heimdall_archive::Journal,
}
impl Index {
    pub fn set_floor_source(&mut self, floor: Arc<std::sync::atomic::AtomicU64>) {
        self.guard.live_floor = Some(floor);
    }
    /// `session` is `<archive>/<session-name>`.
    pub fn open(session: &Path, floor: u64, status: Arc<Mutex<Option<String>>>) -> Result<Self> {
        let root = crate::archive::store_root(session);
        let guard = DiskGuard {
            live_floor: None,
            dir: root.clone(),
            floor,
            status,
        };
        ensure!(
            guard.allows(1024 * 1024),
            "archive index stopped below free-space floor"
        );
        let queue = heimdall_archive::live::Queue::shared(&root)?;
        let evidence = heimdall_archive::Journal::open(session)?;
        let db = heimdall_archive::store::open(&root)?;
        Ok(Self {
            db,
            guard,
            session: evidence.session_id(),
            name: session.file_name().unwrap_or_default().to_string_lossy().into(),
            root: evidence.root().to_path_buf(),
            queue,
            evidence,
        })
    }
    /// Waits until every write handed over so far has landed (tests that read the archive on another connection).
    #[cfg(test)]
    pub(crate) fn settle_for_test(&self) -> bool {
        self.queue.settle(std::time::Duration::from_secs(10))
    }
    /// Live writes still queued behind a held lock.
    #[cfg(test)]
    pub(crate) fn pending_writes(&self) -> usize {
        self.queue.pending()
    }
    #[allow(clippy::too_many_arguments)] // One complete utterance row with its evidence.
    pub fn record(
        &self,
        o: &agent::protocol::Observation,
        source_id: &str,
        start: u64,
        end: u64,
        words: &[listen::Word],
        notes: &[String],
        decision: &Value,
    ) -> Result<()> {
        ensure!(
            self.guard.allows(1024 * 1024),
            "transcript archive stopped below free-space floor"
        );
        let (session, root, journal) = (self.session, self.root.clone(), self.evidence.session_id());
        let (o, source_id, decision) = (o.clone(), source_id.to_owned(), decision.clone());
        let (words, notes) = (serde_json::to_string(words)?, serde_json::to_string(notes)?);
        let binding = heimdall_archive::Record {
            kind: "utterance_binding".into(),
            source: source_id.clone(),
            utterance_ids: vec![o.id.clone()],
            host_ns: start,
            wall_ns: heimdall_archive::wall_ns(),
            payload: json!({"utterance_id":o.id,"listen_id":format!("listen:{source_id}:{start}"),"start_ns":start,"end_ns":end}),
        };
        self.queue.write(move |db| {
            heimdall_archive::timeline::transaction(db, || {
                let projected:Option<String>=db.query_row("SELECT decision FROM utterances WHERE id=?1 AND session=?2",params![o.id,session],|r|r.get(0)).optional()?;
                if projected.as_ref().and_then(|s|serde_json::from_str::<Value>(s).ok()).is_some_and(|v|v["transcript_revision"].as_u64().is_some()) {
                    // A late decision owns disposition, not the recovered transcript.
                    return set_decision(db, session, &o.id, &decision);
                }
                // A final may finish its command work after its independently published
                // send answer. Refresh transcript/attachments without erasing that answer.
                let existing=if decision["status"]=="pending_decision" {
                    db.query_row("SELECT decision FROM utterances WHERE id=?1 AND session=?2",params![o.id,session],|r|r.get::<_,String>(0)).optional()?.map(|s|serde_json::from_str::<Value>(&s)).transpose()?
                }else{None};
                let decision=existing.as_ref().unwrap_or(&decision);
                heimdall_archive::records::insert_event(db, &root, journal, &binding)?;
                db.execute(
                    "INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?12,?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    params![
                        o.id,
                        o.timestamp_ms as i64,
                        source_id,
                        o.source,
                        o.speaker,
                        start as i64,
                        end as i64,
                        o.text,
                        words,
                        notes,
                        decision.to_string(),
                        session
                    ],
                )?;
                heimdall_archive::timeline::utterance_language(db, session, &o.id, &o.language)
            })
        })?;
        Ok(())
    }
    /// Preserve each version in evidence; publish the latest per-speaker rows through
    /// the existing transcript/timeline projection. Never changes action ownership.
    /// False when the final is not archived. Queued behind a held lock, the final is assumed archived: it is
    /// normally one of the writes ahead in the queue, and the queued revision itself still checks.
    pub(crate) fn revise_transcript(&self,id:&str,source:&str,revision:u64,original:&[listen::Word],runs:&[crate::pipeline::speech_split::Run],model_recovery:bool) -> Result<bool> {
        self.revise_transcript_with(id, source, revision, original, runs, model_recovery, true)
    }
    /// Night 8 Nov log sweep, finding 4: `revise_transcript` for a final already archived and projected (its earlier
    /// revision found it), handed to the writer without waiting for its commit (a note's durability: committed with
    /// its group, durable with the next full-sync commit). A voice rename re-projects every final of every call on
    /// the live loop; each waited up to `COMMIT_WAIT` while the writer kept up slowly, and their sum silenced the
    /// live loop 2.0 s (7 Nov 11:38:08 hang file). Always true: the final was found before.
    #[allow(clippy::too_many_arguments)] // `revise_transcript`'s arguments.
    pub(crate) fn revise_projected_transcript(&self,id:&str,source:&str,revision:u64,original:&[listen::Word],runs:&[crate::pipeline::speech_split::Run],model_recovery:bool) -> Result<bool> {
        self.revise_transcript_with(id, source, revision, original, runs, model_recovery, false)
    }
    #[allow(clippy::too_many_arguments)] // `revise_transcript`'s arguments and whether it waits.
    fn revise_transcript_with(&self,id:&str,source:&str,revision:u64,original:&[listen::Word],runs:&[crate::pipeline::speech_split::Run],model_recovery:bool,wait:bool) -> Result<bool> {
        ensure!(self.guard.allows(1024*1024),"transcript revision below disk floor");
        let (session, root, journal) = (self.session, self.root.clone(), self.evidence.session_id());
        let (id, source, original, runs) = (id.to_owned(), source.to_owned(), original.to_vec(), runs.to_vec());
        let found = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = found.clone();
        let job = move |db: &Connection| {
            heimdall_archive::timeline::transaction(db, || {
                let row:Option<(i64,String,String,String)>=db.query_row("SELECT wall_ms,source,notes,decision FROM utterances WHERE session=?1 AND id=?2",params![session,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
                let Some((wall,label,notes,decision))=row else {return Ok(())};
                seen.store(true, std::sync::atomic::Ordering::Release);
                let language: String = db.query_row("SELECT value FROM chunk_tags WHERE chunk_id=?1 AND key='language'", [format!("{session}:text:utterance:{id}")], |r| r.get(0)).optional()?.unwrap_or_default();
                let mut decision:Value=serde_json::from_str(&decision)?;decision["transcript_revision"]=json!(revision);
                let decision=decision.to_string();
                let original_final:Option<Value>=if revision==1 {
                    db.query_row("SELECT id,speaker,start_ns,end_ns,text,words,notes,decision FROM utterances WHERE session=?1 AND id=?2",params![session,id],|r|Ok(json!({"id":r.get::<_,String>(0)?,"speaker":r.get::<_,String>(1)?,"start_ns":r.get::<_,i64>(2)?,"end_ns":r.get::<_,i64>(3)?,"text":r.get::<_,String>(4)?,"words":r.get::<_,String>(5)?,"notes":r.get::<_,String>(6)?,"decision":r.get::<_,String>(7)?}))).optional()?
                }else{None};
                let projection_ids:Vec<_>=(0..runs.len()).map(|i|if i==0 {id.clone()}else{format!("{id}:speech:{i}")}).collect();
                heimdall_archive::records::insert_event(db, &root, journal, &heimdall_archive::Record {
                    kind: "transcript_revision".into(), source: source.clone(), utterance_ids: projection_ids.clone(),
                    host_ns: original.first().map_or(0,|w|w.start_ns), wall_ns: heimdall_archive::wall_ns(),
                    payload: json!({"schema":1,"revision":revision,"previous_revision":revision-1,"original_utterance_id":id,"projection_ids":projection_ids,"original_words":original,"original_final":original_final,"runs":runs,"model_recovery":model_recovery,"actionable":false}),
                })?;
                let prefix=format!("{id}:speech:");
                let mut q=db.prepare("SELECT id FROM utterances WHERE session=?1 AND substr(id,1,?2)=?3")?;
                let old=q.query_map(params![session,prefix.len() as i64,prefix],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
                for child in old {db.execute("DELETE FROM utterances WHERE session=?1 AND id=?2",params![session,child])?;heimdall_archive::timeline::remove_utterance(db,session,&child)?;}
                for (i,run) in runs.iter().enumerate() {
                    let Some(first)=run.words.first() else {continue};let end=run.words.iter().map(|w|w.end_ns).max().unwrap();
                    let child=if i==0 {id.clone()}else{format!("{id}:speech:{i}")};
                    db.execute("INSERT OR REPLACE INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",params![session,child,wall,source,label,run.speaker,first.start_ns as i64,end as i64,run.text(),serde_json::to_string(&run.words)?,if i==0 {notes.as_str()}else{"[]"},decision])?;
                    heimdall_archive::timeline::utterance(db,session,&child)?;
                    heimdall_archive::timeline::utterance_language(db,session,&child,&language)?;
                }
                Ok(())
            })
        };
        if !wait {
            self.queue.enqueue(job)?;
            return Ok(true);
        }
        let written = self.queue.write(job)?;
        Ok(match written {
            heimdall_archive::live::Written::Now => found.load(std::sync::atomic::Ordering::Acquire),
            heimdall_archive::live::Written::Queued => true,
        })
    }
    #[cfg(test)]
    pub(crate) fn languages_for_test(&self) -> Vec<String> {
        self.db.prepare("SELECT value FROM chunk_tags WHERE key='language' AND chunk_id LIKE '%:text:utterance:%' ORDER BY chunk_id").unwrap().query_map([], |r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    }
    /// Complete an already archived final without rebinding its transcript or language.
    pub fn decision(&self, id:&str, decision:&Value) -> Result<()> {
        ensure!(self.guard.allows(1024*1024),"transcript archive stopped below free-space floor");
        let (session, id, decision) = (self.session, id.to_owned(), decision.clone());
        self.queue.write(move |db| heimdall_archive::timeline::transaction(db, || set_decision(db, session, &id, &decision)))?;
        Ok(())
    }
    pub fn event(&self, kind: &str, source: &str, ns: u64, payload: &Value) -> Result<()> {
        self.event_with(kind, source, ns, payload, true)
    }
    /// `event` without its own full disk flush, for the live loop's frequent observations (transcript segments,
    /// capture gaps and discontinuities): see `Journal::note`. State changes (on/off, settings) keep `event`.
    pub fn note(&self, kind: &str, source: &str, ns: u64, payload: &Value) -> Result<()> {
        self.event_with(kind, source, ns, payload, false)
    }
    fn event_with(&self, kind: &str, source: &str, ns: u64, payload: &Value, full_sync: bool) -> Result<()> {
        ensure!(
            self.guard.allows(65536),
            "event archive stopped below free-space floor"
        );
        {
            let ids: Vec<String> = payload
                .get("utterance_id")
                .map(|id| {
                    vec![id.as_str().map(str::to_owned).unwrap_or_else(|| format!("{}:{}", self.name, id))]
                })
                .unwrap_or_default();
            if full_sync {
                self.evidence.event(kind, source, &ids, ns, payload.clone())?;
            } else {
                self.evidence.note(kind, source, &ids, ns, payload.clone())?;
            }
            // The compressed journal owns events; the legacy table remains readable.
        }
        Ok(())
    }
    /// Final utterances overlapping a host-clock window, oldest first, as JSON rows
    /// (words and the decision record parsed).
    pub fn utterances_between(&self, start_ns: u64, end_ns: u64) -> Result<Vec<Value>> {
        // Replay and QA read back what the live loop wrote, including writes queued behind a held lock.
        self.queue.settle(heimdall_archive::store::BUSY);
        let mut stmt = self.db.prepare(
            "SELECT id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision FROM utterances WHERE session=?3 AND end_ns>=?1 AND start_ns<=?2 ORDER BY start_ns",
        )?;
        let rows = stmt.query_map(params![start_ns as i64, end_ns as i64, self.session], |r| {
            let parse = |s: Option<String>| s.and_then(|s| serde_json::from_str::<Value>(&s).ok()).unwrap_or(Value::Null);
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "wall_ms": r.get::<_, i64>(1)?,
                "source_id": r.get::<_, String>(2)?,
                "source": r.get::<_, String>(3)?,
                "speaker": r.get::<_, String>(4)?,
                "start_ns": r.get::<_, i64>(5)?,
                "end_ns": r.get::<_, i64>(6)?,
                "text": r.get::<_, String>(7)?,
                "words": parse(r.get(8)?),
                "notes": parse(r.get(9)?),
                "decision": parse(r.get(10)?),
            }))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
    pub fn relabel(&self, source: &str, ids: &[String], name: &str) -> Result<()> {
        ensure!(
            self.guard.allows(65536),
            "speaker label archive stopped below free-space floor"
        );
        let (session, source, ids, name) = (self.session, source.to_owned(), ids.to_vec(), name.to_owned());
        // Night 8 Nov log sweep, finding 4: a label change on the live loop never waits for its commit (see
        // `revise_projected_transcript`); order with every other write is kept.
        self.queue.enqueue(move |db| heimdall_archive::timeline::transaction(db, || {
            for id in &ids {
                db.execute(
                    "UPDATE utterances SET speaker=?1 WHERE session=?4 AND source_id=?2 AND id=?3",
                    params![name, source, id, session],
                )?;
                heimdall_archive::timeline::utterance(db, session, id)?;
            }
            Ok(())
        }))?;
        Ok(())
    }
}
impl Index {
    /// A voice renamed (a code label named, or a name changed): every archived line and call speaker labelled `from`,
    /// in every session, now says `to`, and their timeline chunks are projected again. Returns the lines found.
    pub fn relabel_speaker(&self, from: &str, to: &str) -> Result<usize> {
        ensure!(
            self.guard.allows(65536),
            "speaker label archive stopped below free-space floor"
        );
        let lines: i64 = self.db.query_row("SELECT count(*) FROM utterances WHERE speaker=?1", params![from], |r| r.get(0))?;
        let (from, to) = (from.to_owned(), to.to_owned());
        // Night 8 Nov log sweep, finding 4: the rename runs on the live loop (`Pipeline::rename_voice`), so it is
        // handed to the writer without waiting (it used to wait up to 30 s for its commit there). The writer keeps
        // the job through a held lock and retries until it commits, in order with every later write; a rename cut
        // short anyway is repeated the same way, since the archive's old label still counts as the voice.
        self.queue.enqueue(move |db| heimdall_archive::timeline::transaction(db, || {
            let rows: Vec<(i64, String)> = db
                .prepare("SELECT session, id FROM utterances WHERE speaker=?1")?
                .query_map(params![from], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            db.execute("UPDATE utterances SET speaker=?2 WHERE speaker=?1", params![from, to])?;
            db.execute("UPDATE timeline_speakers SET name=?2 WHERE name=?1", params![from, to])?;
            for (session, id) in &rows {
                heimdall_archive::timeline::utterance(db, *session, id)?;
            }
            heimdall_archive::timeline::rename_speaker_tag(db, &from, &to)
        }).inspect_err(|e| eprintln!("{}", rename_not_archived(&from, &to, e))))
        .map_err(|e| anyhow::anyhow!("the archive is full or stopped; the rename did not start, try again ({e:#})"))?;
        Ok(lines as usize)
    }
}
/// Sets a final's disposition on it and its speech runs, keeping its transcript revision. Inside a transaction.
fn set_decision(db: &Connection, session: i64, id: &str, decision: &Value) -> Result<()> {
    let previous:Option<String>=db.query_row("SELECT decision FROM utterances WHERE session=?1 AND id=?2",params![session,id],|r|r.get(0)).optional()?;
    let revision=previous.and_then(|s|serde_json::from_str::<Value>(&s).ok()).and_then(|v|v["transcript_revision"].as_u64());
    let mut decision=decision.clone();if let Some(revision)=revision {decision["transcript_revision"]=json!(revision);}
    let prefix = format!("{id}:speech:");
    db.execute("UPDATE utterances SET decision=?5 WHERE session=?1 AND (id=?2 OR substr(id,1,?3)=?4)",params![session,id,prefix.len() as i64,prefix,decision.to_string()])?;
    let mut query = db.prepare("SELECT id FROM utterances WHERE session=?1 AND (id=?2 OR substr(id,1,?3)=?4)")?;
    let ids = query.query_map(params![session,id,prefix.len() as i64,prefix], |r| r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for projection in ids {
        heimdall_archive::timeline::utterance(db,session,&projection)?;
    }
    Ok(())
}
impl Drop for Index {
    fn drop(&mut self) {
        // Quit: give writes queued behind a held lock a bounded chance to land (the Quit limit is 45 s), the live
        // events' writer thread first (`live_event`).
        if !settle_live_events(std::time::Duration::from_secs(5)) {
            eprintln!("Archive live events still queued at exit");
        }
        if !self.queue.settle(std::time::Duration::from_secs(15)) {
            eprintln!("Archive live writes still queued at exit: {}", self.queue.pending());
        }
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
    }
}
/// Live-loop evidence written outside the `Index` (send context, a typed send decision, speak-back), as
/// `Journal::event`. Bug note 2026-11-05T02-53-27-5: the live loop stood 2.1-2.2 s in `Journal::open` (4 Nov 03:36 and
/// 04:19, a new SQLite connection reading its header behind a struggling disk), and each event also waited for its
/// commit. Now the caller only queues the event: one worker thread keeps the session's journal open and writes the
/// events in the order they were queued (`settle_live_events` lets Quit and tests wait for them). A failed write is
/// logged there. Tests write before returning, as before, so a record is readable at once.
pub(crate) fn live_event(session: &Path, kind: &str, source: &str, ids: &[String], host_ns: u64, payload: serde_json::Value) -> Result<()> {
    queue_live_event(session, kind, source, ids, host_ns, payload)?;
    #[cfg(test)]
    settle_live_events(std::time::Duration::from_secs(10));
    Ok(())
}

struct LiveEvent { session: PathBuf, kind: String, source: String, ids: Vec<String>, host_ns: u64, payload: serde_json::Value }

/// Events queued and events written, with a condvar for `settle_live_events`.
struct LiveEvents { tx: crossbeam_channel::Sender<LiveEvent>, counts: Mutex<(u64, u64)>, done: std::sync::Condvar }

#[cfg(test)]
static LIVE_EVENT_GATE: Mutex<()> = Mutex::new(());

fn live_events() -> Result<&'static LiveEvents> {
    static WORKER: std::sync::OnceLock<std::result::Result<LiveEvents, String>> = std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<LiveEvent>();
        std::thread::Builder::new().name("heimdall-live-events".into()).spawn(move || {
            let mut open: Option<(PathBuf, heimdall_archive::Journal)> = None;
            for e in rx {
                #[cfg(test)]
                let _gate = LIVE_EVENT_GATE.lock().unwrap_or_else(|e| e.into_inner());
                let written = (|| -> Result<()> {
                    if open.as_ref().is_none_or(|(at, _)| *at != e.session) {
                        open = None;
                        open = Some((e.session.clone(), heimdall_archive::Journal::open(&e.session)?));
                    }
                    open.as_ref().expect("opened above").1.event(&e.kind, &e.source, &e.ids, e.host_ns, e.payload)
                })();
                if let Err(err) = written {
                    eprintln!("{} not archived: {err:#}", e.kind);
                    // The next event opens the journal again.
                    open = None;
                }
                if let Ok(w) = live_events() {
                    w.counts.lock().unwrap_or_else(|e| e.into_inner()).1 += 1;
                    w.done.notify_all();
                }
            }
        }).map_err(|e| format!("the live event writer did not start: {e}"))?;
        Ok(LiveEvents { tx, counts: Mutex::new((0, 0)), done: std::sync::Condvar::new() })
    }).as_ref().map_err(|e| anyhow::anyhow!("{e}"))
}

/// Queues one event for the writer thread; never opens the archive or waits on it.
fn queue_live_event(session: &Path, kind: &str, source: &str, ids: &[String], host_ns: u64, payload: serde_json::Value) -> Result<()> {
    let w = live_events()?;
    // Counted before it is sent, so a settle that starts now waits for it.
    let mut counts = w.counts.lock().unwrap_or_else(|e| e.into_inner());
    counts.0 += 1;
    let e = LiveEvent { session: session.to_path_buf(), kind: kind.into(), source: source.into(), ids: ids.to_vec(), host_ns, payload };
    if w.tx.send(e).is_err() {
        counts.0 -= 1;
        anyhow::bail!("the live event writer stopped");
    }
    Ok(())
}

/// Waits up to `limit` for every event queued so far to be written. Whether they all were.
pub(crate) fn settle_live_events(limit: std::time::Duration) -> bool {
    let Ok(w) = live_events() else { return true };
    let deadline = std::time::Instant::now() + limit;
    let mut counts = w.counts.lock().unwrap_or_else(|e| e.into_inner());
    let target = counts.0;
    while counts.1 < target {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() { return false }
        counts = w.done.wait_timeout(counts, left).unwrap_or_else(|e| e.into_inner()).0;
    }
    true
}
pub struct ArchiveSearch {
    pub root: PathBuf,
    pub embeddings: Arc<heimdall_archive::embedding::Service>,
}
/// Dropping a cancelled search future interrupts the active SQLite query. The
/// blocking worker also checks between rows, so pending work cannot continue
/// scanning the archive after the MCP request is cancelled.
pub(crate) struct SearchCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    pub(crate) active: Mutex<Option<rusqlite::InterruptHandle>>,
}
pub(crate) struct CancelSearch(pub(crate) Arc<SearchCancellation>);
impl SearchCancellation {
    pub(crate) fn new()->Self{Self{cancelled:std::sync::atomic::AtomicBool::new(false),active:Mutex::new(None)}}
    /// The call that asked has stopped waiting (`CancelSearch` dropped).
    pub(crate) fn cancelled(&self)->bool {self.cancelled.load(std::sync::atomic::Ordering::Acquire)}
}
impl Drop for CancelSearch {
    fn drop(&mut self) {
        self.0.cancelled.store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.0.active.lock().unwrap().as_ref() { handle.interrupt(); }
    }
}
/// Load (or build) the memory-search vector copy on a background thread shortly after start, so the first search
/// does not wait for it. Logs what it did and what it holds.
pub fn warm_vector_copy(root: PathBuf) {
    let _ = std::thread::Builder::new().name("heimdall-vector-copy".into()).spawn(move || {
        let _ = heimdall_night::gate::background_thread();
        std::thread::sleep(std::time::Duration::from_secs(20));
        let index = root.join(heimdall_archive::store::INDEX);
        if !index.is_file() { return; }
        let m = heimdall_archive::embedding::model();
        let key = heimdall_archive::vector_index::Key { model_id: m.id, model_version: m.version, dimensions: m.dimensions };
        match heimdall_archive::vector_index::warm(&index, &key) {
            Ok(line) => eprintln!("memory search: {line}"),
            Err(e) => eprintln!("memory search: vector copy not loaded ahead ({e:#}); the first search loads it"),
        }
    });
}
/// The search log's first half (docs/chunk-index.md, "Search log"): how the candidates were found and what it cost.
pub(crate) fn search_log(s:&heimdall_archive::timeline::SearchStats)->String {
    let mut line=format!("memory search: {} candidates by {}",s.returned,s.path);
    match s.path {
        "memory"=>line.push_str(&format!(" ({} vectors scored in {:.1} ms, {} re-scored exactly in {:.1} ms, copy ready in {:.1} ms{})",
            s.scored,s.scan_ms,s.rescored,s.verify_ms,s.prepare_ms,s.prepared.as_ref().map_or(String::new(),|e|format!(": {e}")))),
        _=>line.push_str(&format!(" in {:.0} ms",s.scan_ms)),
    }
    if let Some(why)=&s.fallback {line.push_str(&format!("; vector copy unavailable, scanned instead: {why}"));}
    line
}
/// Rerank the first `n` worded hits with `service`; every failure keeps the embedding order. Returns the rows and the
/// log's second half.
pub(crate) fn rerank(rows:Vec<heimdall_archive::timeline::Chunk>,query:&str,service:Option<&heimdall_archive::rerank::Service>,n:usize,more:&mut dyn FnMut()->bool)->(Vec<heimdall_archive::timeline::Chunk>,String) {
    use heimdall_archive::rerank;
    if n==0 {return (rows,"rerank off (memory_rerank 0)".into());}
    let Some(service)=service else {return (rows,"rerank skipped: no reranker installed; embedding order".into())};
    if speak::compute_flag::yield_asked() {return (rows,"rerank skipped: Arsenal asked for room; embedding order".into());}
    let scored=rerank::plan(&rows,n);
    if scored.is_empty() {return (rows,"rerank skipped: no hit has words".into());}
    let documents:Vec<String>=scored.iter().map(|&i|rerank::document(&rows[i])).collect();
    match service.score(query,&documents,more) {
        Ok(s)=>{
            let note=format!("reranked {} worded of {} in {:.0} ms{}, model peak {:.2} GB",scored.len(),rows.len(),s.seconds*1e3,
                s.load_seconds.map_or(String::new(),|l|format!(" (model loaded in {:.2} s)",l)),s.peak_bytes as f64/1e9);
            (rerank::apply(rows,&scored,&s.scores),note)
        },
        Err(e)=>(rows,format!("rerank fallback, embedding order: {e:#}")),
    }
}
/// What the agent reads above literal hits that stand in for a paused meaning search.
pub(crate) const MEANING_PAUSED: &str = "Meaning search is paused because this Mac is low on free memory. These hits come from a plain word search of the same archive instead: rows holding words of the question, the most words first, newest first.";
/// `timeline_search` by the words of a semantic query (`mcp_timeline::literal_ranked`), marked as the stand-in.
pub(crate) async fn literal_fallback(root:std::path::PathBuf,mut query:Value,permit:&agent::delivery::DeliveryPermit)->Result<Value> {
    let words=query["vector"]["query"].clone();
    if let Some(o)=query.as_object_mut() {o.remove("vector");}
    query["_words"]=words;
    let mut page=crate::mcp_timeline::search(root,timeline_pages(),query,permit).await?;
    page["search_mode"]=json!("literal_fallback");
    page["notice"]=json!(MEANING_PAUSED);
    Ok(page)
}
pub(crate) fn install_search_cancellation(db: &Connection, cancellation: Arc<SearchCancellation>, permit: agent::delivery::DeliveryPermit) -> Result<()> {
    // InterruptHandle stops an already executing query. The progress hook also
    // covers cancellation just before SQLite starts, when interrupt() is a no-op.
    db.progress_handler(1000, Some(move || cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire) || permit.check().is_err()))?;
    Ok(())
}
#[async_trait]
impl agent::mcp::TranscriptSearch for ArchiveSearch {
    async fn timeline(&self, mut query:Value, permit:&agent::delivery::DeliveryPermit)->Result<Value>{
        if query.get("vector").is_some() {
            // 100 candidates: merging the rows of one moment frees slots for the next distinct moments. With the
            // reranker on, enough more that about `rerank::candidates()` of them have words.
            let n=heimdall_archive::rerank::candidates();
            let limit=if query["vector"]["query"].is_string() && n>0 {heimdall_archive::rerank::fetch_for(n,100)} else {100};
            let mut legacy=json!({"vector":query["vector"],"limit":limit});
            for key in ["kinds","tags"] { if let Some(v)=query.get(key) { legacy[key]=v.clone(); } }
            if let Some(v)=query.get("query") { legacy["text"]=v.clone(); }
            for (key,field) in [("start","start_wall_ns"),("end","end_wall_ns")] { if let Some(v)=query[key].as_str(){legacy[field]=json!(agent::mcp_contract::wall_ns(v)?);} }
            let ranked=match self.chunks(legacy,permit).await {
                Ok(ranked)=>ranked,
                // Meaning search paused for memory (14 Nov VM run, 8 GiB Mac): the same archive by its words instead,
                // and the answer says so.
                Err(e) if heimdall_archive::embedding::memory_paused(&format!("{e:#}")) && query["vector"]["query"].is_string() => {
                    eprintln!("memory search: meaning search paused for low memory ({e:#}); literal word search instead");
                    return literal_fallback(self.root.clone(),query,permit).await;
                },
                Err(e)=>return Err(e),
            };
            let rows=ranked.as_array().unwrap();
            query["_ranked"]=json!(rows.iter().map(|r|json!([r["id"],r["similarity"]])).collect::<Vec<_>>());
            // Reranked: similarity no longer falls down the list, so the page's floor reads the best one (mcp_timeline).
            if rows.iter().any(|r|r.get("rerank").is_some()) { query["_reranked"]=json!(true); }
        }
        crate::mcp_timeline::search(self.root.clone(), timeline_pages(),query,permit).await
    }
    fn resolve_handle(&self, handle: &str) -> Option<String> {
        timeline_pages().resolve(handle).ok()
    }
    async fn chunks(&self, query: Value, permit: &agent::delivery::DeliveryPermit) -> Result<Value> {
        permit.check()?;
        let mut query: heimdall_archive::timeline::Search = serde_json::from_value(query)?;
        let embeddings=self.embeddings.clone();
        let root = self.root.clone(); let permit = permit.clone();
        let cancellation = Arc::new(SearchCancellation { cancelled: std::sync::atomic::AtomicBool::new(false), active: Mutex::new(None) });
        let _cancel_on_drop = CancelSearch(cancellation.clone());
        tokio::task::spawn_blocking(move || -> Result<Value> {
            permit.check()?;
            if !heimdall_archive::store::exists(&root) {
                ensure!(query.vector.is_none(),"no archive embeddings available");
                return Ok(json!([]));
            }
            let mut query_text=None;
            if let Some(vector)=query.vector.as_ref().filter(|v|v.get("query").is_some()) {
                ensure!(vector.as_object().is_some_and(|o|o.len()==1),"semantic vector accepts only query");
                let text=vector["query"].as_str().ok_or_else(||anyhow::anyhow!("vector query must be text"))?;
                query_text=Some(text.to_owned());
                let values=embeddings.query(text,&mut || !cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire) && permit.check().is_ok())?;
                query.vector=Some(serde_json::to_value(values)?);
            }
            let c = heimdall_archive::store::read_index(&root)?;
            { let mut active=cancellation.active.lock().unwrap();
              ensure!(!cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire), "archive search cancelled");
              permit.check()?; *active=Some(c.get_interrupt_handle()); }
            install_search_cancellation(&c,cancellation.clone(),permit.clone())?;
            let (mut rows,stats)=heimdall_archive::timeline::search_with_stats(&c,&query)?;
            permit.check()?;
            ensure!(!cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire), "archive search cancelled");
            let mut log=search_log(&stats);
            if let Some(text)=&query_text {
                let (reranked,note)=rerank(rows,text,heimdall_archive::rerank::installed().as_deref(),heimdall_archive::rerank::candidates(),
                    &mut || !cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire) && permit.check().is_ok() && !speak::compute_flag::yield_asked());
                rows=reranked;log.push_str("; ");log.push_str(&note);
            }
            if query.vector.is_some() {eprintln!("{log}");}
            permit.check()?;
            Ok(serde_json::to_value(rows)?)
        }).await?
    }
}

#[cfg(test)]
mod search_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[test]
    fn cancellation_before_sqlite_starts_cannot_escape_the_interrupt() -> Result<()> {
        for off_on in [false, true] {
            let gate = Arc::new(agent::delivery::DeliveryGate::new(true));
            let permit = gate.permit()?;
            let db = Connection::open_in_memory()?;
            let cancellation = Arc::new(SearchCancellation { cancelled: AtomicBool::new(false), active: Mutex::new(Some(db.get_interrupt_handle())) });
            install_search_cancellation(&db, cancellation.clone(), permit)?;
            if off_on { gate.store(false, Ordering::Release); gate.store(true, Ordering::Release); }
            else { drop(CancelSearch(cancellation)); }
            let error = db.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000000) SELECT sum(x) FROM n", [], |r| r.get::<_, i64>(0)).unwrap_err();
            assert!(matches!(error, rusqlite::Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::OperationInterrupted));
        }
        Ok(())
    }
}

fn timeline_pages()->Arc<crate::mcp_timeline::Pages>{static PAGES:std::sync::OnceLock<Arc<crate::mcp_timeline::Pages>>=std::sync::OnceLock::new();PAGES.get_or_init(||Arc::new(crate::mcp_timeline::Pages::default())).clone()}

#[cfg(test)]
mod live_event_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Bug note 2026-11-05T02-53-27-5: the live loop stood 2.2 s opening the journal for a send-context event. Queuing
    /// returns at once while the writer is stuck (here held at its gate, as behind a slow disk); the events then land
    /// in the order they were queued.
    #[test]
    fn a_live_event_is_queued_without_waiting_on_the_archive_and_lands_in_order() -> Result<()> {
        let t = tempfile::tempdir()?;
        std::fs::create_dir_all(t.path().join("archive"))?;
        let session = t.path().join("archive/session-live");
        let index = Index::open(&session, 0, Arc::new(Mutex::new(None)))?;
        let gate = LIVE_EVENT_GATE.lock().unwrap();
        let at = Instant::now();
        for n in 0..3 {
            queue_live_event(&session, "send_context", "heimdall", &[], n, json!({"n": n}))?;
        }
        assert!(at.elapsed() < Duration::from_millis(50), "queuing waited {:?}", at.elapsed());
        assert!(!settle_live_events(Duration::from_millis(100)), "the writer is held");
        drop(gate);
        assert!(settle_live_events(Duration::from_secs(10)));
        let ns: Vec<Value> = index.evidence.records()?.into_iter().filter(|r| r.kind == "send_context").map(|r| r.payload["n"].clone()).collect();
        assert_eq!(ns, [json!(0), json!(1), json!(2)]);
        Ok(())
    }
}

#[cfg(test)]
mod live_lock_tests {
    //! Review 20 Oct, item 10: the live loop's archive writes under a lock another writer holds for long.
    use super::*;
    use std::time::{Duration, Instant};

    fn words(text: &str, start: u64) -> Vec<listen::Word> {
        text.split(' ').enumerate().map(|(i, w)| listen::Word { text: w.into(), start_ns: start + i as u64 * 300_000_000, end_ns: start + i as u64 * 300_000_000 + 250_000_000, confidence: 0.9 }).collect()
    }
    /// One final's whole life on the live loop: archived pending, decided, split into two speakers, relabelled.
    fn live_sequence(index: &Index) -> Result<Duration> {
        let mut slowest = Duration::ZERO;
        let mut timed = |f: &mut dyn FnMut() -> Result<()>| -> Result<()> { let at = Instant::now(); f()?; slowest = slowest.max(at.elapsed()); Ok(()) };
        let all = words("hey heimdall open the notes", 1_000_000_000);
        let o = agent::protocol::Observation { id: "s:1".into(), timestamp_ms: 1_000, source: "Mic".into(), speaker: "User".into(), language: "en".into(), text: "hey heimdall open the notes".into() };
        timed(&mut || index.record(&o, "mic", 1_000_000_000, 2_500_000_000, &all, &[], &json!({"status":"pending_decision"})))?;
        timed(&mut || index.decision("s:1", &json!({"status":"sent"})))?;
        let runs = vec![
            crate::pipeline::speech_split::Run { speaker: "User".into(), track: None, words: all[..2].to_vec() },
            crate::pipeline::speech_split::Run { speaker: "Anna".into(), track: None, words: all[2..].to_vec() },
        ];
        timed(&mut || { assert!(index.revise_transcript("s:1", "mic", 1, &all, &runs, false)?, "the final counts as archived"); Ok(()) })?;
        timed(&mut || index.relabel("mic", &["s:1:speech:1".into()], "Anna B"))?;
        timed(&mut || index.event("listening_ready", "heimdall", 3_000_000_000, &json!({"ms_since_on": 5})))?;
        Ok(slowest)
    }
    fn rows(index: &Index) -> Result<Vec<Value>> {
        Ok(index.utterances_between(0, u64::MAX / 2)?.into_iter().map(|r| json!({"id":r["id"],"speaker":r["speaker"],"text":r["text"],"decision":r["decision"]})).collect())
    }
    fn kinds(index: &Index) -> Result<Vec<String>> {
        Ok(index.evidence.records()?.into_iter().map(|r| r.kind).filter(|k| k != "timeline_settings").collect())
    }

    #[test]
    fn live_index_writes_never_wait_out_a_held_lock_and_land_in_order() -> Result<()> {
        // Baseline: no contention, every write commits before it returns.
        let free = tempfile::tempdir()?;
        std::fs::create_dir_all(free.path().join("archive"))?;
        let a = Index::open(&free.path().join("archive/session-free"), 0, Arc::new(Mutex::new(None)))?;
        live_sequence(&a)?;
        assert_eq!(a.pending_writes(), 0);
        let (expected, expected_kinds) = (rows(&a)?, kinds(&a)?);
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[1]["speaker"], "Anna B");
        assert_eq!(expected[0]["decision"]["status"], "sent");
        assert_eq!(expected[0]["decision"]["transcript_revision"], 1);
        assert_eq!(expected_kinds, ["utterance_binding", "transcript_revision", "listening_ready"]);

        // Held: compaction or backfill holds the index lock while the same final arrives.
        let held = tempfile::tempdir()?;
        let session = held.path().join("archive/session-held");
        std::fs::create_dir_all(held.path().join("archive"))?;
        let b = Index::open(&session, 0, Arc::new(Mutex::new(None)))?;
        let root = crate::archive::store_root(&session);
        let (locked, wait) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || -> Result<()> {
            let other = heimdall_archive::store::open(&root)?;
            other.execute_batch("BEGIN IMMEDIATE")?;
            locked.send(()).unwrap();
            let _ = released.recv_timeout(Duration::from_secs(30));
            other.execute_batch("COMMIT")?;
            Ok(())
        });
        wait.recv()?;
        let started = Instant::now();
        let slowest = live_sequence(&b)?;
        let wait = heimdall_archive::live::COMMIT_WAIT;
        assert!(slowest < wait + Duration::from_millis(300), "one live write waited {slowest:?}");
        assert!(started.elapsed() < wait + Duration::from_millis(1000), "the whole final took {:?} under the lock", started.elapsed());
        assert_eq!(b.pending_writes(), 5, "every write queued in order behind the first");
        // The decider's archive sink, on its own thread, waits briefly for these rows and finds them settled after.
        assert!(!heimdall_archive::live::settle(&crate::archive::store_root(&session), Duration::from_millis(10)));
        assert!(b.languages_for_test().is_empty(), "nothing lands while the lock is held");
        release.send(())?;
        holder.join().unwrap()?;
        assert_eq!(rows(&b)?, expected, "same rows as without contention: nothing dropped, dependent writes saw earlier ones");
        assert_eq!(kinds(&b)?, expected_kinds);
        assert_eq!(b.pending_writes(), 0);
        Ok(())
    }
    /// 24 Oct 23:37: a listen note's insert read index pages on the live loop for 2.1 s. A note never runs there: with
    /// the full live lock wait and the lock held, it returns at once, queued, and lands in order with later writes.
    #[test]
    fn a_note_is_queued_never_written_on_the_live_loop() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let session = temp.path().join("archive/session-notes");
        std::fs::create_dir_all(temp.path().join("archive"))?;
        let index = Index::open(&session, 0, Arc::new(Mutex::new(None)))?;
        let holder = heimdall_archive::store::open(&crate::archive::store_root(&session))?;
        holder.execute_batch("BEGIN IMMEDIATE")?;
        let started = Instant::now();
        index.evidence.note("listen_event", "mic", &[], 1, json!({"n":1}))?;
        index.note("transcript_segment", "mic", 2, &json!({"n":2}))?;
        assert!(started.elapsed() < Duration::from_millis(100), "a note waited {:?} on the live loop", started.elapsed());
        assert_eq!(index.pending_writes(), 2, "both notes queued");
        holder.execute_batch("COMMIT")?;
        index.event("listening_ready", "heimdall", 3, &json!({"n":3}))?;
        assert!(index.queue.settle(Duration::from_secs(10)));
        assert_eq!(kinds(&index)?, ["listen_event", "transcript_segment", "listening_ready"]);
        Ok(())
    }
}

/// The line a voice rename leaves when the archive's writer could not commit it after the rename was reported done
/// (review, 8 Nov: the rename no longer waits for its commit, so the failure is named here instead).
pub(crate) fn rename_not_archived(from: &str, to: &str, e: &impl std::fmt::Display) -> String {
    format!("voice: rename {from} → {to} not archived ({e}); the archive keeps {from} until the rename is repeated")
}

#[cfg(test)]
mod rerank_tests {
    //! The search path's reranking without a model: off, not installed, failing, and the log line.
    use super::*;
    fn chunk(id: &str, text: &str) -> heimdall_archive::timeline::Chunk {
        serde_json::from_value(json!({"id":id,"session":1,"kind":"text","role":"utterance","source":"mic","start_ns":0,"end_ns":1,
            "start_wall_ns":0,"end_wall_ns":1,"text":text,"details":{},"tags":[],"links":[],"content_hash":"h","similarity":0.8})).unwrap()
    }
    fn ids(rows: &[heimdall_archive::timeline::Chunk]) -> Vec<&str> { rows.iter().map(|c| c.id.as_str()).collect() }

    #[test]
    fn every_way_the_reranker_is_unavailable_keeps_the_embedding_order_and_says_why() {
        let rows = || vec![chunk("a", "one"), chunk("screen", ""), chunk("b", "two")];
        let (out, note) = rerank(rows(), "q", None, 100, &mut || true);
        assert_eq!((ids(&out), note.contains("no reranker installed")), (vec!["a", "screen", "b"], true));
        let (out, note) = rerank(rows(), "q", None, 0, &mut || true);
        assert_eq!((ids(&out), note.as_str()), (vec!["a", "screen", "b"], "rerank off (memory_rerank 0)"));
        // A models folder without the helper or weights: the start fails, the order stays, the reason is logged.
        let empty = tempfile::tempdir().unwrap();
        let service = heimdall_archive::rerank::service(empty.path());
        let (out, note) = rerank(rows(), "q", Some(&service), 100, &mut || true);
        assert_eq!(ids(&out), ["a", "screen", "b"]);
        assert!(note.starts_with("rerank fallback, embedding order:") && note.contains("missing"), "{note}");
        assert!(out.iter().all(|c| c.rerank.is_none()));
        let (_, note) = rerank(vec![chunk("s", "")], "q", Some(&service), 100, &mut || true);
        assert_eq!(note, "rerank skipped: no hit has words");
    }

    #[test]
    fn the_search_log_names_the_path_costs_and_fallback() {
        let stats = heimdall_archive::timeline::SearchStats { path: "memory", scored: 402_337, rescored: 400, returned: 125,
            scan_ms: 5.2, verify_ms: 19.8, prepare_ms: 0.1, prepared: Some("3 changed".into()), ..Default::default() };
        let line = search_log(&stats);
        assert!(line.starts_with("memory search: 125 candidates by memory (402337 vectors scored in 5.2 ms, 400 re-scored exactly in 19.8 ms"), "{line}");
        assert!(line.contains(": 3 changed"), "{line}");
        let scan = heimdall_archive::timeline::SearchStats { path: "scan", scan_ms: 27_000., returned: 100, fallback: Some("disk full".into()), ..Default::default() };
        assert_eq!(search_log(&scan), "memory search: 100 candidates by scan in 27000 ms; vector copy unavailable, scanned instead: disk full");
    }
}
