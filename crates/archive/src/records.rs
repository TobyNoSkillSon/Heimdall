use crate::{hash, store, wall_ns};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

// A raw-content zstd dictionary, frozen/versioned. No external dictionary needed.
pub const DICTIONARY: &[u8] = b"heimdall-archive-dictionary-v1 source source_id utterance_id utterance_ids host_ns start_ns end_ns wall_ns timestamp_ms words text confidence probability end_of_turn_probability addressed_probability context current input output speaker speaker_id embedding labels name decisions latency audio_reference sample_rate samples pcm_sha256 d1_questions d1_state pending_transcript would_wake forced kind transcript vad turn segment command screenshot agent_send agent_reply speak_back schema source_id rate channels frames source label payload image/png image/webp decision error status true false null";
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub kind: String,
    pub source: String,
    pub utterance_ids: Vec<String>,
    pub host_ns: u64,
    pub wall_ns: u64,
    pub payload: Value,
}
/// One session's evidence journal inside the store. `session` is `<archive>/<session-name>`.
pub struct Journal {
    pub(crate) db: Connection,
    root: PathBuf,
    session_path: PathBuf,
    session: i64,
    view: Option<crate::View>,
    /// The store's writer: every write of a journal goes through it (`live.rs`). `None` when opened read-only.
    queue: Option<crate::live::Queue>,
}
thread_local! {
    /// One dictionary compressor per thread that inserts records (the writer): recreating its context per record
    /// wastes CPU at 31 VAD frames a second per source.
    static COMPRESSOR: std::cell::RefCell<Option<zstd::bulk::Compressor<'static>>> = const { std::cell::RefCell::new(None) };
}
fn with_compressor<T>(f: impl FnOnce(&mut zstd::bulk::Compressor<'static>) -> Result<T>) -> Result<T> {
    COMPRESSOR.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() {
            *c = Some(zstd::bulk::Compressor::with_dictionary(1, DICTIONARY)?);
        }
        f(c.as_mut().unwrap())
    })
}
/// Inserts one record inside the caller's transaction (a live job's), and reports it to the diagnostics hooks
/// the way `Journal::event` does: on the thread that applies the job, once the job holds the lock.
pub fn insert_event(db: &Connection, root: &Path, session: i64, record: &Record) -> Result<()> {
    crate::diagnostics::enqueued(record);
    with_compressor(|c| insert_record(db, root, session, record, c))
}
fn append_on(db: &Connection, root: &Path, session: i64, record: &Record, full_sync: bool) -> Result<()> {
    // Inside the live writer's group (a savepoint of its transaction) the group's commit is the full-sync one.
    if full_sync || !db.is_autocommit() {
        return commit_one(db, root, session, record);
    }
    // A note commits without its own F_FULLFSYNC (WAL `synchronous=NORMAL`: safe from an app crash). The next
    // full-sync commit on this index (capture's, about once a second while audio arrives) flushes the WAL file and
    // with it this frame. The connection is back to FULL before anything else runs on it, even on error.
    db.execute_batch("PRAGMA synchronous=NORMAL")?;
    let result = commit_one(db, root, session, record);
    let restored = db.execute_batch("PRAGMA synchronous=FULL");
    result?;
    Ok(restored?)
}
/// One record in its own transaction, or a savepoint of the caller's (the live writer's group).
fn commit_one(db: &Connection, root: &Path, session: i64, record: &Record) -> Result<()> {
    crate::timeline::transaction(db, || with_compressor(|c| insert_record(db, root, session, record, c)))
}
/// Inserts one record (payload CAS, evidence row, VAD activity) inside the caller's transaction.
fn insert_record(db: &Connection, root: &Path, session: i64, record: &Record, compressor: &mut zstd::bulk::Compressor<'_>) -> Result<()> {
    let enriched = crate::diagnostics::enrich(record);
    let record = enriched.as_ref().unwrap_or(record);
    let bytes = serde_json::to_vec(&record.payload)?;
    let id = hash(&bytes);
    let known: bool = db
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM main.payloads WHERE id=?1) OR EXISTS(SELECT 1 FROM main.packed_payloads WHERE id=?1)")?
        .query_row([&id], |r| r.get(0))?;
    if !known {
        match crate::chunks::payload_is_packed(db, root, &id, &record.payload)? {
            // Repeating an old input reuses its existing packed body.
            Some(chunk) => db
                .prepare_cached("INSERT OR IGNORE INTO main.packed_payloads VALUES(?1,?2,?3)")?
                .execute(params![id, bytes.len() as i64, chunk])?,
            None => db
                .prepare_cached("INSERT OR IGNORE INTO main.payloads VALUES(?1,?2,'json-zstd-dict-v1',?3,NULL)")?
                .execute(params![id, bytes.len() as i64, compressor.compress(&bytes)?])?,
        };
    }
    db.prepare_cached("INSERT INTO main.evidence(session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,?2,?3,?4,?5,?6,?7)")?
        .execute(params![
            session,
            record.kind,
            record.source,
            serde_json::to_string(&record.utterance_ids)?,
            record.host_ns as i64,
            record.wall_ns as i64,
            id
        ])?;
    let evidence_id = db.last_insert_rowid();
    record_activity(db, session, record)?;
    crate::timeline::record(db, session, evidence_id, record)?;
    for violation in crate::diagnostics::observe(record, evidence_id) {
        insert_record(db, root, session, &violation, compressor)?;
    }
    Ok(())
}
/// A payload body from its row codec (`json-packed-v1`: read from the chunk `packed_payloads` names).
pub(crate) fn payload_body(db: &Connection, root: &Path, id: &str, raw: usize, codec: &str, data: Vec<u8>, dec: &mut zstd::bulk::Decompressor<'_>) -> Result<Vec<u8>> {
    ensure!(raw <= 64 * 1024 * 1024, "record exceeds decode limit");
    Ok(match codec {
        "json-raw-chunk-v1" => data,
        "json-packed-v1" => crate::chunks::packed_body(db, root, id)?,
        _ => dec.decompress(&data, raw)?,
    })
}
impl Journal {
    /// The session's journal. Its writes go through the store's writer (shared with every other writer of the store
    /// in this process, so their order holds): `event` waits for its commit at most `live::COMMIT_WAIT` while the
    /// writer keeps up, `note` never waits, and neither ever fails on a held lock.
    pub fn open(session: &Path) -> Result<Self> {
        let (root, name) = store::locate(session)?;
        let queue = crate::live::Queue::shared(&root)?;
        let db = store::open(&root)?;
        let id = store::ensure_session(&db, &name)?;
        Ok(Self { db, root, session_path: session.into(), session: id, view: None, queue: Some(queue) })
    }
    /// The session's complete journal, live and packed rows, under the shared lifetime lock.
    pub fn open_read_only(session: &Path) -> Result<Self> {
        // What this process handed to the store's writer is read too.
        crate::live::settle(&store::locate(session)?.0, Duration::from_secs(60));
        let view = crate::View::session(session)?;
        view.evidence()?;
        let (root, _) = store::locate(session)?;
        let db = store::open(&root)?;
        Ok(Self { db, root, session_path: session.into(), session: view.session_id(), view: Some(view), queue: None })
    }
    pub fn session_id(&self) -> i64 {
        self.session
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// A FULL-synchronous (F_FULLFSYNC) commit: returns once it is committed (command authorization is durable
    /// before its action runs), or after `live::COMMIT_WAIT` with the record queued while the writer is behind.
    pub fn append(&self, record: &Record) -> Result<()> {
        self.append_with(record, true)
    }
    fn queue(&self) -> Result<&crate::live::Queue> {
        self.queue.as_ref().context("journal was opened read-only")
    }
    fn append_with(&self, record: &Record, full_sync: bool) -> Result<()> {
        let queue = self.queue()?;
        let (root, session, record) = (self.root.clone(), self.session, record.clone());
        let job = move |db: &Connection| append_on(db, &root, session, &record, full_sync);
        // A note is never waited for: the caller does not read index pages or wait for the writer.
        if full_sync {
            queue.write(job)?;
        } else {
            queue.enqueue(job)?;
        }
        Ok(())
    }
    pub fn vector(&self, values: &[f32]) -> Result<String> {
        ensure!(
            !values.is_empty() && values.len() <= 32768 && values.iter().all(|x| x.is_finite()),
            "invalid model vector"
        );
        let bytes = crate::audio::pcm_bytes(values);
        let id = hash(&bytes);
        let data = zstd::bulk::compress(&bytes, 1)?;
        let (key, n) = (id.clone(), values.len() as i64);
        self.queue()?.write(move |db| {
            crate::timeline::transaction(db, || {
                db.execute("INSERT OR IGNORE INTO main.vectors VALUES(?1,?2,?3)", params![key, n, data])?;
                Ok(())
            })
        })?;
        Ok(id)
    }
    pub fn vector_values(&self, id: &str) -> Result<Vec<f32>> {
        vector_values(&self.db, id)
    }
    pub fn reference_tensors(&self, value: &mut Value) -> Result<()> {
        if let Some(values) = value.get("tensor_values").and_then(Value::as_array) {
            let floats: Vec<f32> = values.iter().map(|v| v.as_f64().map(|x| x as f32)
                .ok_or_else(|| anyhow::anyhow!("invalid tensor value"))).collect::<Result<_>>()?;
            let ids = floats.chunks(32768).map(|v| self.vector(v)).collect::<Result<Vec<_>>>()?;
            value.as_object_mut().unwrap().remove("tensor_values");
            value["vector_ids"] = serde_json::json!(ids);
            value["elements"] = serde_json::json!(floats.len());
        }
        match value {
            Value::Object(map) => for child in map.values_mut() { self.reference_tensors(child)?; },
            Value::Array(array) => for child in array { self.reference_tensors(child)?; },
            _ => (),
        }
        Ok(())
    }
    /// Replace recurring dense outputs with immutable vector references.
    pub fn reference_vectors(&self, payload: &mut Value) -> Result<()> {
        for key in ["current_embedding", "pristine_embedding"] {
            if let Some(values) = payload.get(key).and_then(Value::as_array) {
                let vector: Vec<f32> = values
                    .iter()
                    .map(|x| {
                        x.as_f64()
                            .map(|x| x as f32)
                            .ok_or_else(|| anyhow::anyhow!("nonfloat model vector"))
                    })
                    .collect::<Result<_>>()?;
                let id = self.vector(&vector)?;
                payload.as_object_mut().unwrap().remove(key);
                payload[format!("{key}_id")] = serde_json::json!(id);
            }
        }
        if let Some(values) = payload.get("history_embeddings").and_then(Value::as_array) {
            let mut ids = vec![];
            ensure!(values.len() % 512 == 0, "TCL history vector geometry");
            for v in values.chunks(512) {
                let vector: Vec<f32> = v
                    .iter()
                    .map(|x| {
                        x.as_f64()
                            .map(|x| x as f32)
                            .ok_or_else(|| anyhow::anyhow!("nonfloat history"))
                    })
                    .collect::<Result<_>>()?;
                ids.push(self.vector(&vector)?);
            }
            payload
                .as_object_mut()
                .unwrap()
                .remove("history_embeddings");
            payload["history_embedding_ids"] = serde_json::json!(ids);
        }
        if let Some(values) = payload.get("embeddings").and_then(Value::as_array) {
            let mut ids = vec![];
            for values in values {
                let vector: Vec<f32> = values
                    .as_array()
                    .ok_or_else(|| anyhow::anyhow!("speaker vector shape"))?
                    .iter()
                    .map(|x| {
                        x.as_f64()
                            .map(|x| x as f32)
                            .ok_or_else(|| anyhow::anyhow!("nonfloat embedding"))
                    })
                    .collect::<Result<_>>()?;
                ids.push(self.vector(&vector)?);
            }
            payload.as_object_mut().unwrap().remove("embeddings");
            payload["embedding_ids"] = serde_json::json!(ids);
        }
        Ok(())
    }
    pub fn event(
        &self,
        kind: &str,
        source: &str,
        ids: &[String],
        host_ns: u64,
        payload: Value,
    ) -> Result<()> {
        let record = Record {
            kind: kind.into(),
            source: source.into(),
            utterance_ids: ids.to_vec(),
            host_ns,
            wall_ns: wall_ns(),
            payload,
        };
        crate::diagnostics::enqueued(&record);
        self.append(&record)
    }
    /// `event` for frequent observations nothing waits on (listen events, capture gaps): handed to the writer without
    /// waiting, committed with its next group (milliseconds), and durable against power loss only with the next
    /// full-sync commit on the index (capture commits about once a second, the same bound as its own uncommitted tail). Each F_FULLFSYNC it saves blocked the
    /// live loop 5-25 ms (`lab/opt-1023/REPORT.md`). Command authorizations and anything an action waits on use
    /// `event`.
    pub fn note(&self, kind: &str, source: &str, ids: &[String], host_ns: u64, payload: Value) -> Result<()> {
        let record = Record { kind: kind.into(), source: source.into(), utterance_ids: ids.to_vec(), host_ns, wall_ns: wall_ns(), payload };
        crate::diagnostics::enqueued(&record);
        self.append_with(&record, false)
    }
    pub fn records(&self) -> Result<Vec<Record>> {
        let mut records = vec![];
        self.for_each_record(|r| {
            records.push(r);
            Ok(())
        })?;
        Ok(records)
    }
    /// Every record of the session, live and packed, in append order.
    pub fn for_each_record(&self, consume: impl FnMut(Record) -> Result<()>) -> Result<()> {
        // Notes and events may still wait in the writer: read what this journal wrote, as before.
        if let Some(queue) = &self.queue {
            queue.settle(Duration::from_secs(60));
        }
        let own;
        let view = match &self.view {
            Some(v) => v,
            None => {
                own = crate::View::session(&self.session_path)?;
                &own
            }
        };
        view.for_each_record(consume)
    }
    /// Live (hot) records whose wall clock lies in `[start_wall_ns, end_wall_ns]`, in
    /// append order. Rows already packed into cold chunks are not included; a bug-note
    /// window is minutes old, never cold.
    pub fn records_between_wall(&self, start_wall_ns: u64, end_wall_ns: u64) -> Result<Vec<Record>> {
        let mut stmt = self.db.prepare(
            "SELECT e.kind,e.source,e.utterance_ids,e.host_ns,e.wall_ns,e.payload_id,COALESCE(p.raw_bytes,0),COALESCE(p.data,X''),COALESCE(p.codec,'json-packed-v1') FROM main.evidence e INDEXED BY evidence_time LEFT JOIN main.payloads p ON p.id=e.payload_id WHERE e.wall_ns BETWEEN ?1 AND ?2 AND e.session=?3 ORDER BY e.id",
        )?;
        let mut rows = stmt.query(params![start_wall_ns as i64, end_wall_ns as i64, self.session])?;
        let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
        let mut out = vec![];
        while let Some(r) = rows.next()? {
            let id: String = r.get(5)?;
            let body = payload_body(&self.db, &self.root, &id, r.get::<_, i64>(6)? as usize, &r.get::<_, String>(8)?, r.get(7)?, &mut dec)?;
            out.push(Record {
                kind: r.get(0)?,
                source: r.get(1)?,
                utterance_ids: serde_json::from_str(&r.get::<_, String>(2)?)?,
                host_ns: r.get::<_, i64>(3)? as u64,
                wall_ns: r.get::<_, i64>(4)? as u64,
                payload: serde_json::from_slice(&body)?,
            });
        }
        Ok(out)
    }
    /// Exact encoded-byte hash; perceptual merging would discard text/cursor changes.
    /// The blob goes into the hot pack before the writer's transaction (`Prepare`), outside the index lock.
    pub fn image(&self, bytes: &[u8], mime: &str) -> Result<String> {
        let queue = self.queue()?;
        let id = hash(bytes);
        store::image_key(&id, mime)?;
        let bytes = std::sync::Arc::new(bytes.to_vec());
        let (root, mime) = (self.root.clone(), mime.to_owned());
        let (r, b, m) = (root.clone(), bytes.clone(), mime.clone());
        let prepare: crate::live::Prepare = Box::new(move |db| {
            let _ = prepare_image(db, &r, &b, &m);
        });
        queue.write_prepared(Some(prepare), move |db| store_image(db, &root, &bytes, &mime))?;
        Ok(id)
    }
    pub fn image_bytes(&self, id: &str) -> Result<(String, Vec<u8>)> {
        image_bytes(&self.db, &self.root, id)
    }
}
/// Before the writer's transaction: an image that is new, or held in another encoding (cold WebP), puts its original
/// bytes into the hot pack.
fn prepare_image(db: &Connection, root: &Path, bytes: &[u8], mime: &str) -> Result<()> {
    let id = hash(bytes);
    let stored: Option<(String, String)> = db.query_row("SELECT mime,stored_sha256 FROM main.images WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if !stored.is_some_and(|(m, s)| m == mime && s == id) {
        store::put(root, store::HOT, &[(store::image_key(&id, mime)?, bytes)])?;
    }
    Ok(())
}
/// The image's catalogue row, in the writer's transaction. `ensure_blob` re-checks the hot blob under the index lock
/// (the collector may have removed it since `prepare_image`).
fn store_image(db: &Connection, root: &Path, bytes: &[u8], mime: &str) -> Result<()> {
    let id = hash(bytes);
    let key = store::image_key(&id, mime)?;
    let stored: Option<(String, String, String)> = db
        .query_row("SELECT mime,pack,stored_sha256 FROM main.images WHERE id=?1", [&id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    let unchanged = stored.as_ref().is_some_and(|(m, _, s)| m == mime && *s == id);
    crate::timeline::transaction(db, || {
        if unchanged {
            db.execute("UPDATE main.images SET wall_ns=?2,cold_checked=0 WHERE id=?1", params![id, wall_ns() as i64])?;
            return Ok(());
        }
        store::ensure_blob(root, store::HOT, &key, bytes)?;
        if let Some((old_mime, old_pack, _)) = &stored {
            store::queue_gc(db, old_pack, &store::image_key(&id, old_mime)?)?;
        }
        db.execute(
            "INSERT INTO main.images(id,mime,pack,stored_sha256,wall_ns,cold_checked) VALUES(?1,?2,'hot',?1,?3,0) ON CONFLICT(id) DO UPDATE SET mime=excluded.mime,pack='hot',stored_sha256=excluded.stored_sha256,wall_ns=excluded.wall_ns,cold_checked=0",
            params![id, mime, wall_ns() as i64],
        )?;
        Ok(())
    })
}
pub fn vector_values(db: &Connection, id: &str) -> Result<Vec<f32>> {
    let (n, data): (i64, Vec<u8>) = db.query_row(
        "SELECT dimensions,data FROM main.vectors WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    ensure!((1..=32768).contains(&n), "invalid vector dimensions");
    let bytes = zstd::bulk::decompress(&data, n as usize * 4)?;
    ensure!(hash(&bytes) == id, "vector checksum mismatch");
    crate::audio::pcm_from_bytes(&bytes)
}
/// An image's current bytes and MIME, checksum-verified. A concurrent cold conversion can move
/// the blob between the catalogue read and the blob read; the catalogue is then read again.
pub fn image_bytes(db: &Connection, root: &Path, id: &str) -> Result<(String, Vec<u8>)> {
    let mut attempt = 0;
    loop {
        let (mime, pack, digest): (String, String, String) = db
            .query_row("SELECT mime,pack,stored_sha256 FROM main.images WHERE id=?1", [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .with_context(|| format!("image {id} is not catalogued"))?;
        match store::get(root, &pack, &store::image_key(id, &mime)?)? {
            Some(bytes) => {
                ensure!(hash(&bytes) == digest, "image checksum mismatch");
                return Ok((mime, bytes));
            }
            None if attempt < 2 => attempt += 1,
            None => anyhow::bail!("image {id} missing from pack {pack}"),
        }
    }
}

/// Notes an evidence writer keeps queued behind a held lock or a full disk (a few minutes of four sources' VAD frames,
/// tens of MB). Past it new ones are counted as dropped (`diagnostic_drops`), never an error to the caller.
pub const WRITER_BACKLOG: usize = 65_536;
/// The model-evidence bridge (listen, decisions, speakers): a handle on the store's writer for frequent records nothing
/// waits on. `event` and `diagnostic` never wait and never fail on the archive (31 Oct 22:21: an evidence writer that
/// stopped on a held lock failed every listen event after it); `flush` waits for what was handed over.
#[derive(Clone)]
pub struct Writer {
    queue: crate::live::Queue,
    root: PathBuf,
    session: i64,
    drops: std::sync::Arc<std::sync::atomic::AtomicU64>,
    backlog: usize,
}
impl Writer {
    pub fn open(session: &Path) -> Result<Self> {
        Self::open_bounded(session, WRITER_BACKLOG)
    }
    /// `open` keeping at most `backlog` queued notes (tests saturate a small one).
    pub fn open_bounded(session: &Path, backlog: usize) -> Result<Self> {
        let journal = Journal::open(session)?;
        Ok(Self { queue: journal.queue()?.clone(), root: journal.root, session: journal.session, drops: Default::default(), backlog })
    }
    pub fn event(&self, kind: &str, source: &str, ids: &[String], host_ns: u64, payload: Value) -> Result<()> {
        let record = Record { kind: kind.into(), source: source.into(), utterance_ids: ids.to_vec(), host_ns, wall_ns: wall_ns(), payload };
        // Before the queue: the app log keeps the line even when the archive is stuck or full.
        crate::diagnostics::enqueued(&record);
        self.push(record);
        Ok(())
    }
    /// Sparse diagnostics; dropped and counted like events past the backlog.
    pub fn diagnostic(&self, kind: &str, source: &str, host_ns: u64, payload: Value) {
        let record = Record { kind: kind.into(), source: source.into(), utterance_ids: vec![], host_ns, wall_ns: wall_ns(), payload };
        crate::diagnostics::enqueued(&record);
        self.push(record);
    }
    fn push(&self, record: Record) {
        use std::sync::atomic::Ordering;
        if self.queue.notes() >= self.backlog {
            self.drops.fetch_add(1, Ordering::Relaxed);
            crate::live::lost_write();
            return;
        }
        self.publish_drops();
        self.note(record, true);
    }
    /// Records the notes dropped since the last count (a `diagnostic_drops` record).
    fn publish_drops(&self) {
        let dropped = self.drops.swap(0, std::sync::atomic::Ordering::Relaxed);
        if dropped > 0 {
            let count = Record {
                kind: "diagnostic_drops".into(), source: "heimdall".into(), utterance_ids: vec![], host_ns: 0, wall_ns: wall_ns(),
                payload: serde_json::json!({"count": dropped, "reason": "evidence writer unavailable or queue full"}),
            };
            self.note(count, false);
        }
    }
    fn note(&self, record: Record, prefetch: bool) {
        let (root, session) = (self.root.clone(), self.session);
        // Old chunks a VAD frame may repeat are read before the writer's transaction (chunks::payload_is_packed).
        let prepare: Option<crate::live::Prepare> = prefetch.then(|| {
            let (root, payload) = (root.clone(), record.payload.clone());
            Box::new(move |db: &Connection| {
                let _ = crate::chunks::prefetch(db, &root, &payload);
            }) as crate::live::Prepare
        });
        if self.queue.enqueue_prepared(prepare, move |db| with_compressor(|c| insert_record(db, &root, session, &record, c))).is_err() {
            self.drops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::live::lost_write();
        }
    }
    /// Waits until everything handed over is committed (15 s at most).
    pub fn flush(&self) -> Result<()> {
        self.publish_drops();
        ensure!(self.queue.settle(Duration::from_secs(15)), "archive evidence still queued: {} writes", self.queue.pending());
        Ok(())
    }
}

fn record_activity(db: &Connection, session: i64, r: &Record) -> Result<()> {
    if r.payload["model"] != "silero" {
        return Ok(());
    }
    let Some(start) = r.payload["input"]["start_ns"].as_u64() else {
        return Ok(());
    };
    let Some(end) = r.payload["input"]["end_ns"].as_u64() else {
        return Ok(());
    };
    let Some(p) = r.payload["probability"].as_f64() else {
        return Ok(());
    };
    if !p.is_finite() || end <= start {
        return Ok(());
    }
    let speech = p >= 0.1;
    let prior: Option<(i64, i64, bool)> = db
        .prepare_cached("SELECT start_ns,end_ns,speech FROM main.audio_activity WHERE session=?1 AND source=?2 ORDER BY start_ns DESC LIMIT 1")?
        .query_row(params![session, r.source], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .optional()?;
    if let Some((a, b, s)) = prior {
        if s == speech && start.abs_diff(b as u64) <= 62500 {
            db.prepare_cached("UPDATE main.audio_activity SET end_ns=?4 WHERE session=?1 AND source=?2 AND start_ns=?3")?
                .execute(params![session, r.source, a, end as i64])?;
            return Ok(());
        }
    }
    db.prepare_cached("INSERT OR REPLACE INTO main.audio_activity VALUES(?1,?2,?3,?4,?5)")?
        .execute(params![session, r.source, start as i64, end as i64, speech])?;
    Ok(())
}
/// Low-probability VAD runs of one source, shrunk by the 250 ms speech-quality guards.
pub fn non_speech_ranges_in(db: &Connection, session: i64, source: &str, start: u64, end: u64) -> Result<Vec<(u64, u64)>> {
    let rows = db
        .prepare_cached("SELECT start_ns,end_ns FROM main.audio_activity WHERE session=?1 AND source=?2 AND speech=0 AND end_ns>?3 AND start_ns<?4")?
        .query_map(params![session, source, start as i64, end as i64], |r| {
            Ok((r.get::<_, i64>(0)? as u64 + 250_000_000, (r.get::<_, i64>(1)? as u64).saturating_sub(250_000_000)))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}
/// VAD projections are discarded only after the audio they classify is committed cold, at most `limit` rows per
/// call (one writer job). Returns the rows discarded.
pub fn discard_activity_in(db: &Connection, session: i64, source: &str, end: u64, limit: usize) -> Result<usize> {
    Ok(db
        .prepare_cached("DELETE FROM main.audio_activity WHERE (session,source,start_ns) IN (SELECT session,source,start_ns FROM main.audio_activity WHERE session=?1 AND source=?2 AND end_ns<=?3 LIMIT ?4)")?
        .execute(params![session, source, end as i64, limit as i64])?)
}

#[cfg(test)]
mod durability_tests {
    use super::*;
    #[test]
    fn synchronous_command_authorization_is_visible_before_writer_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("session-durable");
        let journal = Journal::open(&session).unwrap();
        for (pragma, expected) in [("synchronous", 2), ("fullfsync", 1), ("checkpoint_fullfsync", 1)] {
            assert_eq!(journal.db.query_row(&format!("PRAGMA {pragma}"), [], |r| r.get::<_, i32>(0)).unwrap(), expected);
        }
        journal.event("command_decision", "mic:test", &["session:1".into()], 42,
            serde_json::json!({"schema":1,"phase":"chain","approved":"look_at_this"})).unwrap();
        let records = Journal::open_read_only(&session).unwrap().records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].payload["approved"], "look_at_this");
    }
    fn synchronous(journal: &Journal) -> i32 {
        journal.db.query_row("PRAGMA synchronous", [], |r| r.get::<_, i32>(0)).unwrap()
    }
    #[test]
    fn a_note_is_visible_at_once_in_order_and_leaves_the_connection_full_sync() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("session-note");
        let journal = Journal::open(&session).unwrap();
        journal.note("listen_event", "mic:test", &[], 1, serde_json::json!({"n":1})).unwrap();
        assert_eq!(synchronous(&journal), 2, "FULL again after a note");
        journal.event("command_decision", "mic:test", &[], 2, serde_json::json!({"n":2})).unwrap();
        journal.note("capture_gap", "mic:test", &[], 3, serde_json::json!({"n":3})).unwrap();
        assert_eq!(synchronous(&journal), 2);
        let kinds: Vec<_> = Journal::open_read_only(&session).unwrap().records().unwrap().into_iter().map(|r| r.kind).collect();
        assert_eq!(kinds, ["listen_event", "command_decision", "capture_gap"]);
    }
    /// Lab timing, not a check (disk-bound; `lab/opt-1023/REPORT.md`): per-record commit time of `event` vs `note`
    /// on one connection, interleaved in blocks. `cargo test -p heimdall-archive --lib note_commit_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn note_commit_timing() {
        let temp = tempfile::tempdir().unwrap();
        let journal = Journal::open(&temp.path().join("session-timing")).unwrap();
        let payload = serde_json::json!({"event":{"SpeechStart":{"source":"mic:test","host_time_ns":1}}});
        let (mut full, mut note) = (vec![], vec![]);
        for block in 0..10 {
            for i in 0..20u64 {
                let at = std::time::Instant::now();
                if block % 2 == 0 { journal.event("listen_event", "mic:test", &[], i, payload.clone()).unwrap(); full.push(at.elapsed()); }
                else { journal.note("listen_event", "mic:test", &[], i, payload.clone()).unwrap(); note.push(at.elapsed()); }
            }
        }
        let stats = |v: &mut Vec<Duration>| { v.sort(); (v.iter().sum::<Duration>().as_secs_f64() * 1e3 / v.len() as f64, v[v.len() / 2].as_secs_f64() * 1e3, v[v.len() * 95 / 100].as_secs_f64() * 1e3) };
        println!("event (FULL):  mean/median/p95 ms {:?}", stats(&mut full));
        println!("note (NORMAL): mean/median/p95 ms {:?}", stats(&mut note));
    }
    /// A held lock (another process) never fails a note or an event: both wait in the writer, in order, and the event's
    /// caller waits at most `live::COMMIT_WAIT`.
    #[test]
    fn notes_and_events_behind_a_held_lock_never_fail_and_commit_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("session-note-queued");
        let journal = Journal::open(&session).unwrap();
        let holder = rusqlite::Connection::open(journal.root.join(crate::store::INDEX)).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        journal.note("listen_event", "mic:test", &[], 1, serde_json::json!({"n":1})).unwrap();
        let started = std::time::Instant::now();
        journal.event("command_decision", "mic:test", &[], 2, serde_json::json!({"n":2})).unwrap();
        assert!(started.elapsed() < crate::live::COMMIT_WAIT + Duration::from_millis(300), "waited {:?}", started.elapsed());
        let queue = journal.queue().unwrap().clone();
        assert!(queue.pending() > 0, "both queued behind the held lock");
        holder.execute_batch("COMMIT").unwrap();
        assert!(queue.settle(Duration::from_secs(10)));
        let kinds: Vec<_> = Journal::open_read_only(&session).unwrap().records().unwrap().into_iter().map(|r| r.kind).collect();
        assert_eq!(kinds, ["listen_event", "command_decision"]);
    }
}
