//! A reader's view of one session: the per-session schema the readers were written against
//! (`blocks`, `sources`, `gaps`, `evidence`, `payloads`, `utterances`) as TEMP tables holding the
//! session's live and packed rows, under the shared lifetime lock. Only this session's chunks
//! are decoded. Unqualified table names resolve to these TEMP tables first.
use crate::{chunks, records::DICTIONARY, store};
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::{cell::Cell, collections::HashMap, path::{Path, PathBuf}};

pub struct View {
    pub db: Connection,
    root: PathBuf,
    name: String,
    session: i64,
    evidence: Cell<bool>,
    capture: Cell<bool>,
    transcript: Cell<bool>,
    _reader: Option<crate::compact::ReaderLock>,
}
impl View {
    /// `session` is `<archive>/<session-name>`.
    pub fn session(session: &Path) -> Result<Self> {
        Self::open(session, true)
    }
    /// For a caller that already holds the exclusive lifetime lock (migration).
    pub(crate) fn session_locked(session: &Path) -> Result<Self> {
        Self::open(session, false)
    }
    fn open(session: &Path, lock: bool) -> Result<Self> {
        let (root, name) = store::locate(session)?;
        anyhow::ensure!(store::exists(&root), "no archive store at {}", root.display());
        let reader = if lock { Some(crate::compact::reader_lock(&root)?) } else { None };
        let db = store::open(&root)?;
        let id = store::find_session(&db, &name)?.with_context(|| format!("archive has no session {name}"))?;
        Ok(Self {
            db,
            root,
            name,
            session: id,
            evidence: Cell::new(false),
            capture: Cell::new(false),
            transcript: Cell::new(false),
            _reader: reader,
        })
    }
    /// Latest durable conversation checkpoint. Compaction retains this one row
    /// hot; the indexed lookup decodes one payload, independent of history size.
    /// Older builds only wrote this kind for live, current-day sessions.
    pub fn latest_send_context(root: &Path) -> Result<Option<crate::Record>> {
        let _reader = crate::compact::reader_lock(root)?;
        let db = store::open(root)?;
        let mut stmt = db.prepare("SELECT e.source,e.utterance_ids,e.host_ns,e.wall_ns,e.payload_id,coalesce(p.raw_bytes,0),coalesce(p.codec,'json-packed-v1'),coalesce(p.data,X'') FROM main.evidence e LEFT JOIN main.payloads p ON p.id=e.payload_id WHERE e.kind='send_context' ORDER BY e.id DESC LIMIT 1")?;
        let mut rows = stmt.query([])?;
        let Some(r) = rows.next()? else {
            return Ok(None);
        };
        let id: String = r.get(4)?;
        let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
        let body = crate::records::payload_body(
            &db,
            root,
            &id,
            r.get::<_, i64>(5)? as usize,
            &r.get::<_, String>(6)?,
            r.get(7)?,
            &mut dec,
        )?;
        Ok(Some(crate::Record {
            kind: "send_context".into(),
            source: r.get(0)?,
            utterance_ids: serde_json::from_str(&r.get::<_, String>(1)?)?,
            host_ns: r.get::<_, i64>(2)? as u64,
            wall_ns: r.get::<_, i64>(3)? as u64,
            payload: serde_json::from_slice(&body)?,
        }))
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn session_id(&self) -> i64 {
        self.session
    }
    /// Builds `evidence` and `payloads` (every referenced body resolved) on first use.
    pub fn evidence(&self) -> Result<()> { self.evidence_window(0,i64::MAX) }
    /// Bounded evidence window for resumable timeline migration. Only overlapping cold
    /// chunks are decoded; payload bodies are bounded by the existing HAC3 decode limit.
    pub fn evidence_window(&self, after:i64, through:i64) -> Result<()> {
        if self.evidence.get() {
            return Ok(());
        }
        let db = &self.db;
        db.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS evidence(id INTEGER PRIMARY KEY, session INTEGER, kind TEXT, source TEXT, utterance_ids TEXT, host_ns INTEGER, wall_ns INTEGER, payload_id TEXT);
             CREATE TEMP TABLE IF NOT EXISTS payloads(id TEXT PRIMARY KEY, raw_bytes INTEGER, codec TEXT, data BLOB);",
        )?;
        db.execute(
            "INSERT INTO temp.evidence SELECT id,session,kind,source,utterance_ids,host_ns,wall_ns,payload_id FROM main.evidence WHERE session=?1 AND id>?2 AND id<=?3",
            params![self.session,after,through],
        )?;
        let chunk_ids: Vec<i64> = db
            .prepare("SELECT id FROM main.cold_chunks WHERE kind='evidence' AND scope=?1 AND last_id>?2 AND first_id<=?3 ORDER BY first_id")?
            .query_map(params![self.session,after,through], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for chunk in chunk_ids {
            let tables = chunks::load(db, &self.root, chunk)?;
            chunks::insert_rows(db, &tables, "payloads", "temp.payloads")?;
            chunks::insert_rows(db, &tables, "evidence", "temp.evidence")?;
            db.execute("DELETE FROM temp.evidence WHERE id<=?1 OR id>?2",params![after,through])?;
        }
        db.execute(
            "INSERT OR IGNORE INTO temp.payloads SELECT p.id,p.raw_bytes,p.codec,p.data FROM main.payloads p WHERE p.id IN (SELECT payload_id FROM temp.evidence)",
            [],
        )?;
        // Bodies packed with another session's rows: read them from the chunk the placeholder names.
        let placeholders: Vec<(String, i64)> = db
            .prepare("SELECT k.id,k.chunk FROM main.packed_payloads k WHERE k.id IN (SELECT payload_id FROM temp.evidence) AND k.id NOT IN (SELECT id FROM temp.payloads)")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut loaded: HashMap<i64, chunks::Tables> = HashMap::new();
        for (id, chunk) in placeholders {
            anyhow::ensure!(chunk >= 0, "payload {id} is packed without a chunk reference");
            if !loaded.contains_key(&chunk) {
                if loaded.len() > 8 {
                    loaded.clear();
                }
                loaded.insert(chunk, chunks::load(db, &self.root, chunk)?);
            }
            let tables = &loaded[&chunk];
            let mut found = false;
            for (name, cols, rows) in tables {
                if name != "payloads" {
                    continue;
                }
                let data = cols.iter().position(|c| c == "data").context("payload data")?;
                for row in rows {
                    if matches!(&row[0], rusqlite::types::Value::Text(x) if *x == id) {
                        let raw = cols.iter().position(|c| c == "raw_bytes").context("payload size")?;
                        db.execute(
                            "INSERT OR REPLACE INTO temp.payloads VALUES(?1,?2,'json-raw-chunk-v1',?3)",
                            params![id, row[raw], row[data]],
                        )?;
                        found = true;
                    }
                }
            }
            anyhow::ensure!(found, "payload {id} missing from chunk {chunk}");
        }
        self.evidence.set(true);
        Ok(())
    }
    /// Builds `blocks`, `sources` and `gaps` of every capture segment of the session.
    pub fn capture(&self) -> Result<()> { self.capture_window(0,i64::MAX) }
    /// A waveform reader only needs overlapping callback containers, not the entire session.
    /// Use a fresh View for each span; geometry and packed row IDs stay unchanged.
    pub fn capture_span(&self, source:&str, start:u64, end:u64) -> Result<()> {
        self.capture_span_while(source,start,end,&mut ||true)
    }
    pub fn capture_span_while(&self, source:&str, start:u64, end:u64, more:&mut (impl FnMut()->bool + ?Sized)) -> Result<()> {
        anyhow::ensure!(more(),"capture span cancelled");
        anyhow::ensure!(!self.capture.get(),"capture span requires a fresh view");
        self.db.execute_batch("CREATE TEMP TABLE blocks(id INTEGER PRIMARY KEY,segment INTEGER,source_id TEXT,host_ns INTEGER,rate INTEGER,channels INTEGER,frames INTEGER,codec TEXT,discontinuity INTEGER,object_id TEXT,offset_frames INTEGER,gain REAL,wall_ns INTEGER);
            CREATE INDEX temp.view_block_timeline ON blocks(source_id,host_ns);")?;
        self.db.execute("INSERT INTO temp.blocks SELECT b.* FROM main.blocks b JOIN main.segments s ON s.id=b.segment WHERE s.session=?1 AND b.source_id=?2 AND b.host_ns<?4 AND b.host_ns+b.frames*1000000000/b.rate>?3",params![self.session,source,start as i64,end as i64])?;
        anyhow::ensure!(!self.db.query_row("SELECT EXISTS(SELECT 1 FROM main.cold_chunks c JOIN main.segments s ON s.id=c.scope WHERE c.kind='blocks' AND s.session=?1 AND NOT EXISTS(SELECT 1 FROM main.cold_capture_indexed i WHERE i.chunk_id=c.id))",[self.session],|r|r.get::<_,bool>(0))?,"capture span catalogue backfill pending");
        let ids:Vec<i64>=self.db.prepare("SELECT DISTINCT c.id FROM main.cold_chunks c JOIN main.segments s ON s.id=c.scope JOIN main.cold_capture_spans r ON r.chunk_id=c.id WHERE c.kind='blocks' AND s.session=?1 AND r.source=?2 AND r.start_ns<?4 AND r.end_ns>?3 ORDER BY c.id")?.query_map(params![self.session,source,start as i64,end as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for id in ids {
            anyhow::ensure!(more(),"capture span cancelled");
            chunks::insert_rows(&self.db,&chunks::load(&self.db,&self.root,id)?,"blocks","temp.blocks")?;
            self.db.execute("DELETE FROM temp.blocks WHERE source_id<>?1 OR host_ns>=?3 OR host_ns+frames*1000000000/rate<=?2",params![source,start as i64,end as i64])?;
        }
        self.capture.set(true);Ok(())
    }
    /// Bounded callback-row window used by model-free timeline catch-up.
    pub fn capture_window(&self,after:i64,through:i64) -> Result<()> {
        if self.capture.get() {
            return Ok(());
        }
        let db = &self.db;
        db.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS blocks(id INTEGER PRIMARY KEY, segment INTEGER, source_id TEXT, host_ns INTEGER, rate INTEGER, channels INTEGER, frames INTEGER, codec TEXT, discontinuity INTEGER, object_id TEXT, offset_frames INTEGER, gain REAL, wall_ns INTEGER);
             CREATE INDEX IF NOT EXISTS temp.view_block_timeline ON blocks(source_id, host_ns);
             CREATE TEMP TABLE IF NOT EXISTS sources(segment INTEGER, id TEXT, metadata_json TEXT, added_ns INTEGER, removed_ns INTEGER, PRIMARY KEY(segment, id));
             CREATE TEMP TABLE IF NOT EXISTS gaps(id INTEGER PRIMARY KEY, segment INTEGER, source_id TEXT, host_ns INTEGER, detail TEXT);",
        )?;
        let segments = self.segments()?;
        for (segment, _) in &segments {
            db.execute("INSERT INTO temp.sources SELECT segment,id,metadata_json,added_ns,removed_ns FROM main.sources WHERE segment=?1", [segment])?;
            db.execute("INSERT INTO temp.gaps SELECT id,segment,source_id,host_ns,detail FROM main.gaps WHERE segment=?1", [segment])?;
            db.execute(
                "INSERT INTO temp.blocks SELECT id,segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,gain,wall_ns FROM main.blocks WHERE segment=?1 AND id>?2 AND id<=?3",
                params![segment,after,through],
            )?;
            let chunk_ids: Vec<i64> = db
                .prepare("SELECT id FROM main.cold_chunks WHERE kind='blocks' AND scope=?1 AND last_id>?2 AND first_id<=?3 ORDER BY first_id")?
                .query_map(params![segment,after,through], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for chunk in chunk_ids {
                let tables = chunks::load(db, &self.root, chunk)?;
                chunks::insert_rows(db, &tables, "blocks", "temp.blocks")?;
                db.execute("DELETE FROM temp.blocks WHERE id<=?1 OR id>?2",params![after,through])?;
            }
        }
        self.capture.set(true);
        Ok(())
    }
    /// Builds `utterances` (the session's final transcript projection).
    pub fn transcript(&self) -> Result<()> {
        if self.transcript.get() {
            return Ok(());
        }
        self.db.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS utterances(id TEXT PRIMARY KEY, wall_ms INTEGER, source_id TEXT, source TEXT, speaker TEXT, start_ns INTEGER, end_ns INTEGER, text TEXT, words TEXT, notes TEXT, decision TEXT);",
        )?;
        self.db.execute(
            "INSERT INTO temp.utterances SELECT id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision FROM main.utterances WHERE session=?1",
            [self.session],
        )?;
        self.transcript.set(true);
        Ok(())
    }
    /// Capture segments of the session: (id, name), oldest first.
    pub fn segments(&self) -> Result<Vec<(i64, String)>> {
        Ok(self
            .db
            .prepare("SELECT id,name FROM main.segments WHERE session=?1 ORDER BY id")?
            .query_map([self.session], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }
    pub fn is_complete(&self) -> Result<bool> {
        Ok(self
            .db
            .query_row("SELECT complete_wall_ns IS NOT NULL FROM main.sessions WHERE id=?1", [self.session], |r| r.get(0))?)
    }
    /// Every record of the session in append order.
    pub fn for_each_record(&self, mut consume: impl FnMut(crate::Record) -> Result<()>) -> Result<()> {
        self.for_each_record_identified(|_, record| consume(record))
    }
    /// Stable evidence IDs survive cold packing and identify derived timeline links.
    pub fn for_each_record_identified(&self, mut consume: impl FnMut(i64, crate::Record) -> Result<()>) -> Result<()> {
        self.scan_records(0, |id, record| {consume(id,record)?; Ok(true)})
    }
    /// Resume after a stable ID; returning false stops decoding immediately.
    pub fn scan_records(&self, after: i64, consume: impl FnMut(i64, crate::Record) -> Result<bool>) -> Result<()> {
        self.scan_record_kinds(after,"",consume)
    }
    /// A comma-delimited kind allowlist (including leading/trailing commas). Empty means all.
    pub fn scan_record_kinds(&self, after:i64, kinds:&str, mut consume:impl FnMut(i64,crate::Record)->Result<bool>)->Result<()> {
        self.evidence()?;
        let mut stmt = self.db.prepare(
            "SELECT e.kind,e.source,e.utterance_ids,e.host_ns,e.wall_ns,p.id,p.raw_bytes,p.data,p.codec,e.id FROM temp.evidence e JOIN temp.payloads p ON p.id=e.payload_id WHERE e.id>?1 AND (?2='' OR instr(?2,','||e.kind||',')>0) ORDER BY e.id",
        )?;
        let mut rows = stmt.query(params![after,kinds])?;
        let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
        let mut n = 0usize;
        while let Some(r) = rows.next()? {
            let id: String = r.get(5)?;
            let body = crate::records::payload_body(&self.db, &self.root, &id, r.get::<_, i64>(6)? as usize, &r.get::<_, String>(8)?, r.get(7)?, &mut dec)?;
            if !consume(r.get(9)?, crate::Record {
                kind: r.get(0)?,
                source: r.get(1)?,
                utterance_ids: serde_json::from_str(&r.get::<_, String>(2)?)?,
                host_ns: r.get::<_, i64>(3)? as u64,
                wall_ns: r.get::<_, i64>(4)? as u64,
                payload: serde_json::from_slice(&body)?,
            })? { return Ok(()); }
            n += 1;
        }
        let total: i64 = self.db.query_row("SELECT count(*) FROM temp.evidence WHERE id>?1 AND (?2='' OR instr(?2,','||kind||',')>0)", params![after,kinds], |r| r.get(0))?;
        anyhow::ensure!(n as i64 == total, "{} evidence rows have no payload body", total - n as i64);
        Ok(())
    }
}
