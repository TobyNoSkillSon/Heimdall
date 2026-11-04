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
        ensure!(self.guard.allows(1024*1024),"transcript revision below disk floor");
        let (session, root, journal) = (self.session, self.root.clone(), self.evidence.session_id());
        let (id, source, original, runs) = (id.to_owned(), source.to_owned(), original.to_vec(), runs.to_vec());
        let found = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = found.clone();
        let written = self.queue.write(move |db| {
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
        })?;
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
        self.queue.write(move |db| heimdall_archive::timeline::transaction(db, || {
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
        // The caller saves the speaker store after this: the archive must have the new label first. A rename is
        // the user's own action, rare; it waits for its commit as on main (30 s), never for the writer's whole queue.
        self.queue.call_within(std::time::Duration::from_secs(30), move |db| heimdall_archive::timeline::transaction(db, || {
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
        }))
        .map_err(|e| anyhow::anyhow!("the archive is busy; the rename did not finish, try again ({e:#})"))?;
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
        // Quit: give writes queued behind a held lock a bounded chance to land (the Quit limit is 45 s).
        if !self.queue.settle(std::time::Duration::from_secs(15)) {
            eprintln!("Archive live writes still queued at exit: {}", self.queue.pending());
        }
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
    }
}
/// Live-loop evidence written outside the `Index` (send context, a typed send decision, speak-back), as
/// `Journal::event`: it shares the store's live queue, so it never waits out a held lock either. The session's journal
/// stays open: opening one opens a connection to the index and reads its header and schema, on the live loop for every
/// context update (4 Nov 03:36 hang report: the live loop stood 2.2 s in that open, behind the disk).
pub(crate) fn live_event(session: &Path, kind: &str, source: &str, ids: &[String], host_ns: u64, payload: serde_json::Value) -> Result<()> {
    static OPEN: std::sync::Mutex<Option<(PathBuf, heimdall_archive::Journal)>> = std::sync::Mutex::new(None);
    // In use by another thread (its event waits up to `live::COMMIT_WAIT`): this one opens its own, as before.
    let mut open = match OPEN.try_lock() {
        Ok(open) => open,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return heimdall_archive::Journal::open(session)?.event(kind, source, ids, host_ns, payload),
    };
    if open.as_ref().is_none_or(|(at, _)| at != session) {
        *open = None;
        *open = Some((session.to_path_buf(), heimdall_archive::Journal::open(session)?));
    }
    open.as_ref().expect("opened above").1.event(kind, source, ids, host_ns, payload)
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
impl SearchCancellation { pub(crate) fn new()->Self{Self{cancelled:std::sync::atomic::AtomicBool::new(false),active:Mutex::new(None)}} }
impl Drop for CancelSearch {
    fn drop(&mut self) {
        self.0.cancelled.store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.0.active.lock().unwrap().as_ref() { handle.interrupt(); }
    }
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
            // 100 candidates: merging the rows of one moment frees slots for the next distinct moments.
            let mut legacy=json!({"vector":query["vector"],"limit":100});
            for key in ["kinds","tags"] { if let Some(v)=query.get(key) { legacy[key]=v.clone(); } }
            if let Some(v)=query.get("query") { legacy["text"]=v.clone(); }
            for (key,field) in [("start","start_wall_ns"),("end","end_wall_ns")] { if let Some(v)=query[key].as_str(){legacy[field]=json!(agent::mcp_contract::wall_ns(v)?);} }
            let ranked=self.chunks(legacy,permit).await?;
            query["_ranked"]=json!(ranked.as_array().unwrap().iter().map(|r|json!([r["id"],r["similarity"]])).collect::<Vec<_>>());
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
            if let Some(vector)=query.vector.as_ref().filter(|v|v.get("query").is_some()) {
                ensure!(vector.as_object().is_some_and(|o|o.len()==1),"semantic vector accepts only query");
                let text=vector["query"].as_str().ok_or_else(||anyhow::anyhow!("vector query must be text"))?;
                let values=embeddings.query(text,&mut || !cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire) && permit.check().is_ok())?;
                query.vector=Some(serde_json::to_value(values)?);
            }
            let c = Connection::open_with_flags(root.join(heimdall_archive::store::INDEX), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            c.busy_timeout(std::time::Duration::from_secs(5))?;
            { let mut active=cancellation.active.lock().unwrap();
              ensure!(!cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire), "archive search cancelled");
              permit.check()?; *active=Some(c.get_interrupt_handle()); }
            install_search_cancellation(&c,cancellation.clone(),permit.clone())?;
            let rows=heimdall_archive::timeline::search(&c,&query)?;
            permit.check()?;
            ensure!(!cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire), "archive search cancelled");
            Ok(serde_json::to_value(rows)?)
        }).await?
    }

    async fn search(&self, query: &str, limit: usize, permit: &agent::delivery::DeliveryPermit) -> Result<Value> {
        permit.check()?;
        let root = self.root.clone();
        let query = query.to_owned();
        let permit = permit.clone();
        let cancellation = Arc::new(SearchCancellation { cancelled: std::sync::atomic::AtomicBool::new(false), active: Mutex::new(None) });
        let _cancel_on_drop = CancelSearch(cancellation.clone());
        tokio::task::spawn_blocking(move||->Result<Value>{
            let check = || -> Result<()> {
                ensure!(!cancellation.cancelled.load(std::sync::atomic::Ordering::Acquire), "archive search cancelled");
                permit.check()
            };
            let mut rows=vec![];
            if heimdall_archive::store::exists(&root) {
                // One query over every session's transcript in the store.
                let c=Connection::open(root.join(heimdall_archive::store::INDEX))?;c.execute_batch("PRAGMA query_only=ON")?;
                c.busy_timeout(std::time::Duration::from_secs(5))?;
                {
                    let mut active = cancellation.active.lock().unwrap();
                    check()?;
                    *active = Some(c.get_interrupt_handle());
                }
                install_search_cancellation(&c, cancellation.clone(), permit.clone())?;
                let mut stmt=c.prepare("SELECT id,wall_ms,source,speaker,text,notes FROM utterances INDEXED BY utterance_time WHERE instr(lower(text),lower(?1))>0 ORDER BY wall_ms DESC LIMIT ?2")?;
                for v in stmt.query_map(params![query,limit as i64],|r|Ok(json!({"id":r.get::<_,String>(0)?,"timestamp_ms":r.get::<_,i64>(1)?,"source":r.get::<_,String>(2)?,"speaker":r.get::<_,String>(3)?,"text":r.get::<_,String>(4)?,"notes":r.get::<_,String>(5)?})))? {check()?;rows.push(v?);}
                cancellation.active.lock().unwrap().take();
            }
            check()?;
            rows.sort_by_key(|v|std::cmp::Reverse(v["timestamp_ms"].as_i64().unwrap_or(0)));rows.truncate(limit);Ok(json!(rows))
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
