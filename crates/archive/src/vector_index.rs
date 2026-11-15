//! Semantic search over a separate, memory-mapped copy of the timeline embeddings ("the embeddings
//! should be stored separately from the data itself ... we can just simply check on the embeddings").
//!
//! The SQLite index keeps every vector as zstd-compressed float32 (`vectors`), the source of truth. This module keeps
//! a search-side copy: each current vector normalised and stored as 8-bit integers with one float32 scale per vector
//! (the user, 14:45: "store it at 8 bits or half precision"), in a side file `vector-cache/<key>.hvx` beside the index,
//! memory-mapped (clean file pages: the OS can drop them under pressure, they never go to swap). A query scores every
//! entry with an int8 dot product, keeps the best few hundred, and `timeline::search` re-scores those exactly from
//! SQLite's float32 vectors, so the returned order and similarities are today's (measured on the 60-question set:
//! identical top 10 for 60/60 with a 400-row re-score pool, lab/vector-index/README.md).
//!
//! Kept current by `chunk_embedding_changes`, a trigger-fed log of embedding writes and deletes (timeline::SCHEMA):
//! each search first applies the log rows after the copy's sequence number to an in-RAM delta. A full rebuild from
//! SQLite runs when the delta grows past `REBUILD_DELTA` entries or the copy is older than `REBUILD_AGE`, on a
//! background thread; searches use the old copy meanwhile.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAGIC: &[u8; 4] = b"HVX1";
const END: &[u8; 4] = b"HVXE";
const FORMAT: u32 = 1;
/// Delta entries before a background rebuild folds them into the side file.
pub const REBUILD_DELTA: usize = 20_000;
/// A copy older than this is rebuilt (it also drops vectors whose chunks changed without a new embedding).
pub const REBUILD_AGE: Duration = Duration::from_secs(3 * 86_400);
/// Indexes kept by the process (one per archive and model in practice; tests open many).
const KEEP: usize = 4;
const REBUILD_RETRY: Duration = Duration::from_secs(600);
pub const KINDS: [&str; 5] = ["text", "speech_audio", "other_audio", "moment", "screen"];
pub fn kind_code(kind: &str) -> Option<u8> { KINDS.iter().position(|k| *k == kind).map(|i| i as u8) }

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key { pub model_id: String, pub model_version: String, pub dimensions: usize }
impl Key {
    /// One file per database and model: a shared cache folder never hands one archive another's copy.
    fn file_name(&self, db_path: &Path) -> String {
        let db = db_path.canonicalize().unwrap_or_else(|_| db_path.to_owned());
        let digest = crate::hash(format!("{}\0{}\0{}\0{}", self.model_id, self.model_version, self.dimensions, db.display()).as_bytes());
        format!("{}.hvx", &digest[..16])
    }
    fn bytes(&self) -> Vec<u8> { format!("{}\0{}\0{}", self.model_id, self.model_version, self.dimensions).into_bytes() }
}

/// One vector as stored in the copy: normalised, `round(x / scale)` with `scale = max|x| / 127`.
pub fn quantize(values: &[f32]) -> (f32, Vec<i8>) {
    let norm = values.iter().map(|v| (*v as f64).powi(2)).sum::<f64>().sqrt();
    if !(norm > 0.) || !norm.is_finite() { return (0., vec![0; values.len()]); }
    let max = values.iter().map(|v| (*v as f64 / norm).abs()).fold(0., f64::max);
    if max == 0. { return (0., vec![0; values.len()]); }
    let scale = max / 127.;
    (scale as f32, values.iter().map(|v| ((*v as f64 / norm) / scale).round().clamp(-127., 127.) as i8).collect())
}

#[inline]
fn dot(a: &[i8], b: &[i8]) -> i32 {
    // Lanes of 16 so LLVM vectorises the widening multiply-add.
    let mut acc = [0i32; 16];
    let (ca, cb) = (a.chunks_exact(16), b.chunks_exact(16));
    let tail: i32 = ca.remainder().iter().zip(cb.remainder()).map(|(x, y)| *x as i32 * *y as i32).sum();
    for (x, y) in ca.zip(cb) {
        for i in 0..16 { acc[i] += x[i] as i32 * y[i] as i32; }
    }
    acc.iter().sum::<i32>() + tail
}

/// The immutable part: the side file's bytes (mapped) or, without a cache directory, the same layout in RAM.
enum Bytes { Map(memmap2::Mmap), Owned(Vec<u8>) }
impl std::ops::Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] { match self { Bytes::Map(m) => m, Bytes::Owned(v) => v } }
}
struct Base {
    bytes: Bytes,
    count: usize,
    dims: usize,
    seq: i64,
    built_wall_ns: u64,
    offsets: usize,
    ids: usize,
    kinds: usize,
    sessions: usize,
    scales: usize,
    vectors: usize,
}
fn u32_at(b: &[u8], at: usize) -> u32 { u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) }
fn i64_at(b: &[u8], at: usize) -> i64 { i64::from_le_bytes(b[at..at + 8].try_into().unwrap()) }
fn f32_at(b: &[u8], at: usize) -> f32 { f32::from_le_bytes(b[at..at + 4].try_into().unwrap()) }
impl Base {
    fn parse(bytes: Bytes, key: &Key) -> Result<Self> {
        let b = &bytes[..];
        ensure!(b.len() >= 36 && &b[..4] == MAGIC && u32_at(b, 4) == FORMAT, "not a vector copy");
        let dims = u32_at(b, 8) as usize;
        let count = u32_at(b, 12) as usize;
        let seq = i64_at(b, 16);
        let built_wall_ns = i64_at(b, 24) as u64;
        let key_len = u32_at(b, 32) as usize;
        ensure!(dims == key.dimensions && b.len() >= 36 + key_len && b[36..36 + key_len] == key.bytes()[..], "vector copy is for another model");
        let offsets = 36 + key_len;
        ensure!(b.len() >= offsets + 4 * (count + 1), "vector copy truncated");
        let ids = offsets + 4 * (count + 1);
        let id_bytes = u32_at(b, offsets + 4 * count) as usize;
        let kinds = ids + id_bytes;
        let sessions = kinds + count;
        let scales = sessions + 8 * count;
        let vectors = scales + 4 * count;
        ensure!(b.len() == vectors + count * dims + 4 && &b[b.len() - 4..] == END, "vector copy truncated");
        let base = Self { bytes, count, dims, seq, built_wall_ns, offsets, ids, kinds, sessions, scales, vectors };
        // Ids are sorted (binary search) and their offsets ordered: a damaged file fails here, not mid-search.
        let mut last = 0;
        for i in 0..=count {
            let o = u32_at(&base.bytes, offsets + 4 * i) as usize;
            ensure!(o >= last && o <= id_bytes, "vector copy offsets damaged");
            last = o;
        }
        for i in 1..count { ensure!(base.id(i - 1) < base.id(i), "vector copy ids unsorted"); }
        Ok(base)
    }
    fn id(&self, i: usize) -> &str {
        let a = u32_at(&self.bytes, self.offsets + 4 * i) as usize;
        let b = u32_at(&self.bytes, self.offsets + 4 * (i + 1)) as usize;
        std::str::from_utf8(&self.bytes[self.ids + a..self.ids + b]).unwrap_or("")
    }
    fn kind(&self, i: usize) -> u8 { self.bytes[self.kinds + i] }
    fn session(&self, i: usize) -> i64 { i64_at(&self.bytes, self.sessions + 8 * i) }
    fn scale(&self, i: usize) -> f32 { f32_at(&self.bytes, self.scales + 4 * i) }
    fn vector(&self, i: usize) -> &[i8] {
        let s = &self.bytes[self.vectors + i * self.dims..self.vectors + (i + 1) * self.dims];
        // SAFETY: i8 and u8 have the same size and alignment.
        unsafe { std::slice::from_raw_parts(s.as_ptr() as *const i8, s.len()) }
    }
    fn find(&self, id: &str) -> Option<usize> {
        let (mut lo, mut hi) = (0, self.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.id(mid).cmp(id) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(mid),
            }
        }
        None
    }
    fn heap_bytes(&self) -> usize { match &self.bytes { Bytes::Owned(v) => v.len(), Bytes::Map(_) => 0 } }
}

struct Row { id: String, kind: u8, session: i64, scale: f32, vector: Vec<i8> }

fn encode(key: &Key, seq: i64, built_wall_ns: u64, mut rows: Vec<Row>) -> Vec<u8> {
    rows.sort_by(|a, b| a.id.cmp(&b.id));
    rows.dedup_by(|a, b| a.id == b.id);
    let kb = key.bytes();
    let id_bytes: usize = rows.iter().map(|r| r.id.len()).sum();
    let mut out = Vec::with_capacity(40 + kb.len() + 4 * (rows.len() + 1) + id_bytes + rows.len() * (13 + key.dimensions) + 4);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT.to_le_bytes());
    out.extend_from_slice(&(key.dimensions as u32).to_le_bytes());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&(built_wall_ns as i64).to_le_bytes());
    out.extend_from_slice(&(kb.len() as u32).to_le_bytes());
    out.extend_from_slice(&kb);
    let mut at = 0u32;
    for r in &rows { out.extend_from_slice(&at.to_le_bytes()); at += r.id.len() as u32; }
    out.extend_from_slice(&at.to_le_bytes());
    for r in &rows { out.extend_from_slice(r.id.as_bytes()); }
    for r in &rows { out.push(r.kind); }
    for r in &rows { out.extend_from_slice(&r.session.to_le_bytes()); }
    for r in &rows { out.extend_from_slice(&r.scale.to_le_bytes()); }
    for r in &rows { out.extend(r.vector.iter().map(|v| *v as u8)); }
    out.extend_from_slice(END);
    out
}

fn has_log(db: &Connection) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM main.sqlite_master WHERE type='table' AND name='chunk_embedding_changes')", [], |r| r.get(0))?)
}
fn current_seq(db: &Connection) -> Result<i64> {
    if !has_log(db)? { return Ok(0); }
    Ok(db.query_row("SELECT COALESCE(max(seq),0) FROM main.chunk_embedding_changes", [], |r| r.get(0))?)
}
fn row_from(id: String, kind: &str, session: i64, dims: i64, data: &[u8], key: &Key) -> Result<Row> {
    ensure!(dims as usize == key.dimensions, "stored embedding dimension mismatch");
    let bytes = zstd::bulk::decompress(data, dims as usize * 4)?;
    let values = crate::audio::pcm_from_bytes(&bytes)?;
    let (scale, vector) = quantize(&values);
    Ok(Row { id, kind: kind_code(kind).context("unknown chunk kind")?, session, scale, vector })
}
/// Every current embedding of `key`, read from SQLite (the only full read; about 4 s for 400k chunks, measured).
fn read_all(db: &Connection, key: &Key) -> Result<(i64, Vec<Row>)> {
    // The sequence first: a change committed during the read is applied again by the next refresh (idempotent).
    let seq = current_seq(db)?;
    let mut stmt = db.prepare("SELECT c.id,c.kind,c.session,v.dimensions,v.data FROM main.chunk_embeddings e JOIN main.timeline_chunks c ON c.id=e.chunk_id AND c.content_hash=e.content_hash JOIN main.vectors v ON v.id=e.vector_id WHERE e.model_id=?1 AND e.model_version=?2 AND e.dimensions=?3")?;
    let mut rows = stmt.query(params![key.model_id, key.model_version, key.dimensions as i64])?;
    let mut out = vec![];
    while let Some(r) = rows.next()? {
        let data: Vec<u8> = r.get(4)?;
        out.push(row_from(r.get(0)?, &r.get::<_, String>(1)?, r.get(2)?, r.get(3)?, &data, key)?);
    }
    Ok((seq, out))
}

/// What one `get` did to the copy, for the search log.
#[derive(Debug, Default, Clone)]
pub struct Prepared { pub event: Option<String>, pub ms: f64 }

pub struct Index {
    key: Key,
    file: Option<PathBuf>,
    db_path: PathBuf,
    base: Base,
    /// Base rows replaced or removed by the delta.
    superseded: Vec<bool>,
    delta: Vec<Row>,
    delta_pos: HashMap<String, usize>,
    /// Delta rows removed since (their chunk lost its embedding).
    delta_gone: Vec<bool>,
    seq: i64,
    rebuilding: bool,
    /// The last background rebuild failed at this time: no new one for `REBUILD_RETRY`.
    rebuild_failed: Option<Instant>,
}

/// One coarse candidate: approximate cosine from the 8-bit copy, and the chunk id.
#[derive(Debug, Clone)]
pub struct Candidate { pub approx: f32, pub id: String, pub kind: u8 }

/// Which rows a query may rank, decided in memory before scoring.
#[derive(Default)]
pub struct Filter { pub kinds: Vec<u8>, pub session: Option<i64>, pub ids: Option<std::collections::HashSet<String>> }

impl Index {
    pub fn entries(&self) -> usize { self.base.count - self.superseded.iter().filter(|s| **s).count() + self.delta_gone.iter().filter(|g| !**g).count() }
    /// Bytes this copy holds: mapped file pages (evictable, never swapped) and heap (delta, flags, RAM-only base).
    pub fn memory(&self) -> (usize, usize) {
        let mapped = match &self.base.bytes { Bytes::Map(m) => m.len(), Bytes::Owned(_) => 0 };
        let heap = self.base.heap_bytes() + self.superseded.len() + self.delta_gone.len()
            + self.delta.iter().map(|r| r.id.len() * 2 + r.vector.len() + 64).sum::<usize>();
        (mapped, heap)
    }
    fn from_rows(key: &Key, db_path: &Path, file: Option<PathBuf>, seq: i64, rows: Vec<Row>) -> Result<Self> {
        let bytes = encode(key, seq, crate::wall_ns(), rows);
        let base = match &file {
            Some(path) => match write_file(path, &bytes).and_then(|_| map(path)).and_then(|m| Base::parse(Bytes::Map(m), key)) {
                Ok(base) => base,
                Err(e) => {
                    eprintln!("vector search copy not saved ({e:#}); kept in memory");
                    Base::parse(Bytes::Owned(bytes), key)?
                }
            },
            None => Base::parse(Bytes::Owned(bytes), key)?,
        };
        Ok(Self::with_base(key, db_path, file, base))
    }
    fn with_base(key: &Key, db_path: &Path, file: Option<PathBuf>, base: Base) -> Self {
        Self { key: key.clone(), file, db_path: db_path.to_owned(), superseded: vec![false; base.count], seq: base.seq, base,
            delta: vec![], delta_pos: HashMap::new(), delta_gone: vec![], rebuilding: false, rebuild_failed: None }
    }
    /// Apply embedding writes logged after the copy's sequence number. Returns how many chunks changed.
    fn refresh(&mut self, db: &Connection) -> Result<usize> {
        if !has_log(db)? { return Ok(0); }
        let changed: Vec<(i64, String)> = db.prepare_cached("SELECT seq,chunk_id FROM main.chunk_embedding_changes WHERE seq>?1 ORDER BY seq")?
            .query_map([self.seq], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let Some(last) = changed.last().map(|c| c.0) else { return Ok(0) };
        let mut stmt = db.prepare_cached("SELECT c.kind,c.session,v.dimensions,v.data FROM main.chunk_embeddings e JOIN main.timeline_chunks c ON c.id=e.chunk_id AND c.content_hash=e.content_hash JOIN main.vectors v ON v.id=e.vector_id WHERE e.chunk_id=?1 AND e.model_id=?2 AND e.model_version=?3 AND e.dimensions=?4")?;
        for (_, id) in &changed {
            let row = stmt.query_row(params![id, self.key.model_id, self.key.model_version, self.key.dimensions as i64], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, Vec<u8>>(3)?))
            }).optional()?;
            if let Some(b) = self.base.find(id) { self.superseded[b] = true; }
            match row {
                Some((kind, session, dims, data)) => {
                    let row = row_from(id.clone(), &kind, session, dims, &data, &self.key)?;
                    match self.delta_pos.get(id) {
                        Some(&p) => { self.delta[p] = row; self.delta_gone[p] = false; }
                        None => { self.delta_pos.insert(id.clone(), self.delta.len()); self.delta.push(row); self.delta_gone.push(false); }
                    }
                }
                None => if let Some(&p) = self.delta_pos.get(id) { self.delta_gone[p] = true; },
            }
        }
        self.seq = last;
        Ok(changed.len())
    }
    /// The `pool` best rows by approximate cosine, best first (ties by id). `query` need not be normalised.
    pub fn top(&self, query: &[f32], pool: usize, filter: &Filter) -> Result<Vec<Candidate>> {
        ensure!(query.len() == self.key.dimensions, "query dimension mismatch");
        let (qscale, q) = quantize(query);
        ensure!(qscale > 0., "zero embedding vector");
        let allowed_base = |i: usize| -> bool {
            !self.superseded[i]
                && (filter.kinds.is_empty() || filter.kinds.contains(&self.base.kind(i)))
                && filter.session.is_none_or(|s| s == self.base.session(i))
                && filter.ids.as_ref().is_none_or(|set| set.contains(self.base.id(i)))
        };
        // Score in parallel slices of the mapped copy; a few threads cover 400k x 768 in milliseconds.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 4);
        let n = self.base.count;
        let per = n.div_ceil(threads).max(1);
        let mut scored: Vec<(f32, u32)> = std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads).map(|t| {
                let (a, b) = ((t * per).min(n), ((t + 1) * per).min(n));
                let q = &q;
                let allowed = &allowed_base;
                s.spawn(move || {
                    let mut out = Vec::with_capacity(b - a);
                    for i in a..b {
                        if !allowed(i) { continue; }
                        let score = dot(q, self.base.vector(i)) as f32 * self.base.scale(i) * qscale;
                        out.push((score, i as u32));
                    }
                    out
                })
            }).collect();
            handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        let mut out: Vec<Candidate> = Vec::with_capacity(pool * 2);
        if scored.len() > pool {
            // Ties broken by position, which is id order (the copy is sorted by id), so equal scores at the cut keep
            // the same rows as the scan's id order and every deeper round extends the same ranking.
            scored.select_nth_unstable_by(pool, |a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
            scored.truncate(pool);
        }
        out.extend(scored.into_iter().map(|(approx, i)| Candidate { approx, id: self.base.id(i as usize).to_owned(), kind: self.base.kind(i as usize) }));
        for (p, r) in self.delta.iter().enumerate() {
            if self.delta_gone[p] || !(filter.kinds.is_empty() || filter.kinds.contains(&r.kind))
                || !filter.session.is_none_or(|s| s == r.session) || !filter.ids.as_ref().is_none_or(|set| set.contains(&r.id)) { continue; }
            out.push(Candidate { approx: dot(&q, &r.vector) as f32 * r.scale * qscale, id: r.id.clone(), kind: r.kind });
        }
        out.sort_by(|a, b| b.approx.total_cmp(&a.approx).then_with(|| a.id.cmp(&b.id)));
        out.truncate(pool);
        Ok(out)
    }
    /// The best `pool` rows after skipping `skip` (for a second, deeper pool when filters dropped too many).
    pub fn top_after(&self, query: &[f32], skip: usize, pool: usize, filter: &Filter) -> Result<Vec<Candidate>> {
        let mut all = self.top(query, skip + pool, filter)?;
        Ok(all.split_off(skip.min(all.len())))
    }
}

fn map(path: &Path) -> Result<memmap2::Mmap> {
    let file = std::fs::File::open(path)?;
    // SAFETY: the copy is only ever replaced by rename, never written in place, so the mapped inode is immutable.
    Ok(unsafe { memmap2::Mmap::map(&file)? })
}
fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("side file folder")?;
    crate::private_dir(dir)?;
    // A temp file of its own per write: two builds at once never write into each other's (or a mapped) inode.
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::Write::write_all(&mut temp, bytes)?;
    temp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

type Shared = Arc<Mutex<Option<Index>>>;
fn registry() -> &'static Mutex<Vec<(PathBuf, Key, Shared)>> {
    static R: OnceLock<Mutex<Vec<(PathBuf, Key, Shared)>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(vec![]))
}
/// Where the side file lives: `vector-cache/` beside the index, or `HEIMDALL_VECTOR_CACHE` (a directory, or `off`
/// for RAM only: lab runs against the user's live archive never write into it).
fn cache_file(db_path: &Path, key: &Key) -> Option<PathBuf> {
    let dir = match std::env::var("HEIMDALL_VECTOR_CACHE") {
        Ok(v) if v == "off" => return None,
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => db_path.parent()?.join("vector-cache"),
    };
    Some(dir.join(key.file_name(db_path)))
}
fn db_path(db: &Connection) -> Option<PathBuf> {
    db.path().filter(|p| !p.is_empty()).map(PathBuf::from)
}

/// Run `body` with the current copy for this database and model: loaded from the side file or built from SQLite on
/// first use, then refreshed from the change log. `None` for a database without a file (in-memory tests).
pub fn with<T>(db: &Connection, key: &Key, body: impl FnOnce(&Index, &Prepared) -> Result<T>) -> Result<Option<T>> {
    let Some(path) = db_path(db) else { return Ok(None) };
    let shared = {
        let mut r = registry().lock().unwrap();
        if let Some(p) = r.iter().position(|(p, k, _)| *p == path && k == key) {
            let entry = r.remove(p);
            let shared = entry.2.clone();
            r.push(entry);
            shared
        } else {
            let shared: Shared = Arc::new(Mutex::new(None));
            r.push((path.clone(), key.clone(), shared.clone()));
            if r.len() > KEEP { r.remove(0); }
            shared
        }
    };
    // A panic in an earlier search must not end vector search: the copy itself is never left half-changed by one.
    let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    let started = Instant::now();
    let mut prepared = Prepared::default();
    // A database replaced under the copy (restore, migration) restarts its sequence: load or build afresh.
    if guard.as_ref().is_some_and(|i| i.seq > 0) && current_seq(db)? < guard.as_ref().unwrap().seq { *guard = None; }
    if guard.is_none() {
        let file = cache_file(&path, key);
        let loaded = file.as_ref().filter(|f| f.is_file()).and_then(|f| {
            map(f).and_then(|m| Base::parse(Bytes::Map(m), key))
                .map_err(|e| eprintln!("vector search copy unusable ({e:#}); rebuilding")).ok()
        });
        let seq = current_seq(db)?;
        *guard = Some(match loaded.filter(|b| b.seq <= seq) {
            Some(base) => {
                prepared.event = Some(format!("loaded {} vectors from the side file", base.count));
                Index::with_base(key, &path, file, base)
            }
            None => {
                let (seq, rows) = read_all(db, key)?;
                prepared.event = Some(format!("built {} vectors from the index", rows.len()));
                Index::from_rows(key, &path, file, seq, rows)?
            }
        });
    }
    let index = guard.as_mut().unwrap();
    let changed = index.refresh(db)?;
    if changed > 0 {
        let note = format!("{changed} changed");
        prepared.event = Some(prepared.event.map_or(note.clone(), |e| format!("{e}, {note}")));
    }
    let stale = index.delta.len() > REBUILD_DELTA
        || Duration::from_nanos(crate::wall_ns().saturating_sub(index.base.built_wall_ns)) > REBUILD_AGE;
    if stale && !index.rebuilding && index.rebuild_failed.is_none_or(|t| t.elapsed() >= REBUILD_RETRY) {
        index.rebuilding = true;
        spawn_rebuild(shared.clone(), index.db_path.clone(), key.clone(), index.file.clone());
    }
    prepared.ms = started.elapsed().as_secs_f64() * 1e3;
    body(index, &prepared).map(Some)
}

fn spawn_rebuild(shared: Shared, db_path: PathBuf, key: Key, file: Option<PathBuf>) {
    let _ = std::thread::Builder::new().name("vector-copy-rebuild".into()).spawn(move || {
        let result = (|| -> Result<Index> {
            let db = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            db.busy_timeout(Duration::from_secs(5))?;
            let (seq, rows) = read_all(&db, &key)?;
            Index::from_rows(&key, &db_path, file, seq, rows)
        })();
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        match result {
            Ok(fresh) => *guard = Some(fresh),
            Err(e) => {
                eprintln!("vector search copy rebuild failed: {e:#}");
                if let Some(i) = guard.as_mut() { i.rebuilding = false; i.rebuild_failed = Some(Instant::now()); }
            }
        }
    });
}

/// Build or load the copy ahead of the first search (app start, background thread). Returns what it did.
pub fn warm(index_path: &Path, key: &Key) -> Result<String> {
    let db = Connection::open_with_flags(index_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_secs(5))?;
    let started = Instant::now();
    let line = with(&db, key, |index, prepared| {
        let (mapped, heap) = index.memory();
        Ok(format!("vector search copy ready in {:.2}s: {} ({} entries, {:.0} MB mapped, {:.1} MB heap)",
            started.elapsed().as_secs_f64(), prepared.event.clone().unwrap_or_else(|| "already loaded".into()),
            index.entries(), mapped as f64 / 1e6, heap as f64 / 1e6))
    })?;
    line.context("index has no file")
}

/// Forget every loaded copy (tests, and a lab run that swaps archives).
pub fn reset() { registry().lock().unwrap().clear(); }
/// Forget the copies of one database (after a failed search: the next one loads or builds afresh).
pub fn reset_one(db: &Connection) {
    if let Some(path) = db_path(db) { registry().lock().unwrap().retain(|(p, _, _)| *p != path); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quantized_dot_tracks_cosine() {
        let a: Vec<f32> = (0..768).map(|i| ((i * 37 % 101) as f32 - 50.) / 50.).collect();
        let b: Vec<f32> = (0..768).map(|i| ((i * 53 % 89) as f32 - 44.) / 44.).collect();
        let exact = crate::timeline::cosine(&a, &b).unwrap();
        let (sa, qa) = quantize(&a);
        let (sb, qb) = quantize(&b);
        let approx = dot(&qa, &qb) as f64 * sa as f64 * sb as f64;
        assert!((exact - approx).abs() < 0.01, "{exact} vs {approx}");
        assert_eq!(quantize(&[0., 0.]).0, 0.);
    }
    #[test]
    fn encoded_copy_round_trips_sorted_and_rejects_damage() {
        let key = Key { model_id: "m".into(), model_version: "1".into(), dimensions: 2 };
        let rows = vec![
            Row { id: "b".into(), kind: 1, session: 7, scale: 0.5, vector: vec![1, -2] },
            Row { id: "a".into(), kind: 0, session: 3, scale: 0.25, vector: vec![3, 4] },
        ];
        let bytes = encode(&key, 9, 11, rows);
        let base = Base::parse(Bytes::Owned(bytes.clone()), &key).unwrap();
        assert_eq!((base.count, base.seq, base.id(0), base.id(1)), (2, 9, "a", "b"));
        assert_eq!((base.kind(1), base.session(1), base.scale(1), base.vector(1)), (1, 7, 0.5, &[1i8, -2][..]));
        assert_eq!(base.find("b"), Some(1));
        assert_eq!(base.find("c"), None);
        let other = Key { model_version: "2".into(), ..key.clone() };
        assert!(Base::parse(Bytes::Owned(bytes.clone()), &other).is_err());
        assert!(Base::parse(Bytes::Owned(bytes[..bytes.len() - 1].to_vec()), &key).is_err());
    }
}
