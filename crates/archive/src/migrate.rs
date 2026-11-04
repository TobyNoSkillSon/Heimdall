//! Moves legacy per-launch folders (`archive/session-*/`) and `archive/voice-bank/` into the store.
//! Each session is copied, packed, verified against digests of the legacy rows and blobs, recorded
//! in `migrations`, and only then removed. See docs/archive.md, "Migration from per-launch folders".
use crate::{audio, chunks, hash, legacy::Container, records::DICTIONARY, store, wall_ns};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Keep verified legacy folders instead of removing them.
    pub keep: bool,
}
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Report {
    pub name: String,
    pub verified: bool,
    pub removed: bool,
    /// Legacy rows per table (equal to the store's rows when verified).
    pub rows: BTreeMap<String, u64>,
    pub legacy_bytes: u64,
    pub blobs: u64,
    pub blob_bytes: u64,
    pub digest: String,
    pub seconds: f64,
    pub error: Option<String>,
}

/// Free space kept clear while a folder exists twice (the app's default capture floor is 10 GiB).
const RESERVE: u64 = 10 * 1024 * 1024 * 1024;
const BUSY: &str = "archive store is busy";
const IN_USE: &str = "legacy folder is in use";
impl Report {
    /// Not attempted: the store or the folder was held by another owner.
    pub fn busy(&self) -> bool {
        matches!(self.error.as_deref(), Some(BUSY) | Some(IN_USE))
    }
}
/// Order-independent multiset digest: count and the 256-bit wrapping sum of row SHA-256s.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct Tally {
    count: u64,
    sum: [u64; 4],
}
impl Tally {
    fn add(&mut self, row: &serde_json::Value) {
        let digest = Sha256::digest(serde_json::to_vec(row).unwrap());
        let mut carry = 0u128;
        for (i, limb) in self.sum.iter_mut().enumerate() {
            let part = u64::from_le_bytes(digest[i * 8..i * 8 + 8].try_into().unwrap()) as u128;
            let total = *limb as u128 + part + carry;
            *limb = total as u64;
            carry = total >> 64;
        }
        self.count += 1;
    }
    fn hex(&self) -> String {
        format!("{}:{:016x}{:016x}{:016x}{:016x}", self.count, self.sum[3], self.sum[2], self.sum[1], self.sum[0])
    }
}
#[derive(Default)]
struct Digest_ {
    tables: BTreeMap<&'static str, Tally>,
}
impl Digest_ {
    fn add(&mut self, table: &'static str, row: serde_json::Value) {
        self.tables.entry(table).or_default().add(&row);
    }
    fn combined(&self) -> String {
        let mut h = Sha256::new();
        for (t, tally) in &self.tables {
            h.update(t.as_bytes());
            h.update(tally.hex().as_bytes());
        }
        format!("{:x}", h.finalize())
    }
}

fn is_legacy_session(entry: &std::fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|t| t.is_dir()) && entry.file_name().to_string_lossy().starts_with("session-")
}
fn legacy_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.is_dir() {
        return Ok(vec![]);
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(is_legacy_session)
        .map(|e| e.path())
        .collect();
    dirs.sort();
    let bank = root.join("voice-bank");
    if bank.is_dir() {
        dirs.push(bank);
    }
    Ok(dirs)
}
/// Whether legacy folders remain under `root`.
pub fn pending(root: &Path) -> Result<bool> {
    Ok(!legacy_dirs(root)?.is_empty())
}
/// Migrates every legacy folder. Errors in one session are reported in its report.
pub fn migrate_all(root: &Path, options: &Options) -> Result<Vec<Report>> {
    migrate_all_with(root, options, |_| {})
}
/// `migrate_all`, handing each report to `done` as soon as its folder is finished.
pub fn migrate_all_with(root: &Path, options: &Options, mut done: impl FnMut(&Report)) -> Result<Vec<Report>> {
    let mut out = vec![];
    let mut failed = HashSet::new();
    loop {
        let next = legacy_dirs(root)?.into_iter().find(|d| !failed.contains(d) && !(options.keep && verified(root, d).unwrap_or(false)));
        let Some(dir) = next else { break };
        let report = if looks_live(&dir)? {
            Report {
                name: dir.file_name().unwrap().to_string_lossy().into(),
                error: Some("written to in the last two minutes and not marked complete: quit the running app, then rerun".into()),
                ..Default::default()
            }
        } else {
            migrate_dir(root, &dir, options)?
        };
        if !report.verified || (options.keep && dir.exists()) {
            failed.insert(dir);
        }
        done(&report);
        out.push(report);
    }
    Ok(out)
}
/// Migrates the oldest legacy folder (the compactor's step). `None` when there is none or the
/// store is busy.
/// A folder whose migration failed is retried after an hour, not on every pass.
pub fn migrate_next(root: &Path, options: &Options) -> Result<Option<Report>> {
    static FAILED: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, std::time::Instant>>> = std::sync::OnceLock::new();
    let failed = FAILED.get_or_init(Default::default);
    for dir in legacy_dirs(root)? {
        if looks_live(&dir)?
            || failed.lock().unwrap().get(&dir).is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(3600))
        {
            continue;
        }
        let report = migrate_dir(root, &dir, options)?;
        if report.verified || report.busy() {
            failed.lock().unwrap().remove(&dir);
        } else {
            failed.lock().unwrap().insert(dir, std::time::Instant::now());
        }
        return Ok(Some(report));
    }
    Ok(None)
}
fn verified(root: &Path, dir: &Path) -> Result<bool> {
    if !store::exists(root) {
        return Ok(false);
    }
    let db = store::open(root)?;
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    Ok(db
        .query_row("SELECT EXISTS(SELECT 1 FROM migrations WHERE name=?1)", [name], |r| r.get(0))?)
}
/// A folder written to in the last two minutes may still belong to a running pre-store app.
fn looks_live(dir: &Path) -> Result<bool> {
    if dir.join("COMPLETE").is_file() {
        return Ok(false);
    }
    let now = std::time::SystemTime::now();
    for e in std::fs::read_dir(dir)? {
        let e = e?;
        if matches!(e.file_name().to_str(), Some(".compact.lock" | ".compact.gate")) {
            continue;
        }
        if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
            if now.duration_since(m).is_ok_and(|d| d.as_secs() < 120) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn dir_bytes(p: &Path) -> Result<u64> {
    let mut n = 0;
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            n += dir_bytes(&e.path())?;
        } else if t.is_file() {
            n += e.metadata()?.len();
        }
    }
    Ok(n)
}
fn migrate_dir(root: &Path, dir: &Path, options: &Options) -> Result<Report> {
    let started = std::time::Instant::now();
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let mut report = Report { name: name.clone(), ..Default::default() };
    let Some(_owner) = crate::compact::owner(root)? else {
        report.error = Some(BUSY.into());
        return Ok(report);
    };
    // A pre-store compactor or reader of this folder must not run alongside.
    let legacy_lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join(".compact.lock"));
    let _legacy_lock = match legacy_lock {
        Ok(f) if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&f), libc::LOCK_EX | libc::LOCK_NB) } == 0 => f,
        _ => {
            report.error = Some(IN_USE.into());
            return Ok(report);
        }
    };
    let db = store::open(root)?;
    let done: Option<String> = db
        .query_row("SELECT receipt FROM migrations WHERE name=?1", [&name], |r| r.get(0))
        .optional()?;
    if let Some(receipt) = done {
        // Verified earlier; only the removal was interrupted.
        let earlier: serde_json::Value = serde_json::from_str(&receipt)?;
        report.verified = true;
        report.digest = earlier["digest"].as_str().unwrap_or_default().into();
        if !options.keep {
            std::fs::remove_dir_all(dir)?;
            report.removed = true;
        }
        return Ok(report);
    }
    report.legacy_bytes = dir_bytes(dir)?;
    // The copy exists twice until verification; keep the app's default free-space floor clear.
    let needed = report.legacy_bytes.saturating_add(RESERVE);
    let free = free_bytes(root).unwrap_or(u64::MAX);
    if free < needed {
        report.error = Some(format!("needs {needed} free bytes (the folder again plus a 10 GiB reserve); {free} free"));
        return Ok(report);
    }
    let is_bank = name == "voice-bank";
    let result = if is_bank {
        migrate_bank(root, &db, dir, &mut report)
    } else {
        migrate_session(root, &db, dir, &mut report)
    };
    match result {
        Ok(()) => {
            report.verified = true;
            db.execute(
                "INSERT INTO migrations VALUES(?1,(SELECT id FROM sessions WHERE name=?1),?2,?3)",
                params![name, wall_ns() as i64, serde_json::to_string(&report)?],
            )?;
            db.execute("UPDATE sessions SET migrated_wall_ns=?2 WHERE name=?1", params![name, wall_ns() as i64])?;
            crate::compact::crash_point("migrate_verified");
            if !options.keep {
                std::fs::remove_dir_all(dir)?;
                report.removed = true;
            }
        }
        Err(e) => {
            report.error = Some(format!("{e:#}"));
            if !is_bank {
                let _ = purge_session(&db, &name);
            }
        }
    }
    report.seconds = started.elapsed().as_secs_f64();
    Ok(report)
}
fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(unsafe { libc::statvfs(c.as_ptr(), &mut v) } == 0, "statvfs");
    Ok((v.f_bavail as u64).saturating_mul(v.f_frsize))
}
/// Removes a partially migrated session's own rows; shared content stays (it is content-keyed).
fn purge_session(db: &Connection, name: &str) -> Result<()> {
    let Some(session) = store::find_session(db, name)? else { return Ok(()) };
    let _raised = crate::live::Raised::new();
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<()> {
        let segments: Vec<i64> = db
            .prepare("SELECT id FROM segments WHERE session=?1")?
            .query_map([session], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut chunk_rows: Vec<(i64, String, String)> = db
            .prepare("SELECT id,pack,sha256 FROM cold_chunks WHERE kind='evidence' AND scope=?1")?
            .query_map([session], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for s in &segments {
            chunk_rows.extend(
                db.prepare("SELECT id,pack,sha256 FROM cold_chunks WHERE kind='blocks' AND scope=?1")?
                    .query_map([s], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            db.execute("DELETE FROM blocks WHERE segment=?1", [s])?;
            db.execute("DELETE FROM sources WHERE segment=?1", [s])?;
            db.execute("DELETE FROM gaps WHERE segment=?1", [s])?;
        }
        // A payload body packed into one of these chunks may be shared with other sessions:
        // bring it back into its row before the chunk goes.
        let root = store::root_of(db)?;
        let mut enc = zstd::bulk::Compressor::with_dictionary(19, DICTIONARY)?;
        for (id, _, _) in &chunk_rows {
            let held: i64 = db.query_row("SELECT count(*) FROM packed_payloads WHERE chunk=?1", [id], |r| r.get(0))?;
            if held == 0 {
                continue;
            }
            for (name, cols, rows) in chunks::load(db, &root, *id)? {
                if name != "payloads" {
                    continue;
                }
                let data = cols.iter().position(|c| c == "data").context("payload data")?;
                for row in rows {
                    if let (Value::Text(pid), Value::Blob(body)) = (&row[0], &row[data]) {
                        let held: Option<i64> = db
                            .prepare_cached("SELECT raw_bytes FROM packed_payloads WHERE id=?1 AND chunk=?2")?
                            .query_row(params![pid, id], |r| r.get(0))
                            .optional()?;
                        if let Some(raw) = held {
                            db.prepare_cached("INSERT OR REPLACE INTO payloads VALUES(?1,?2,'json-zstd-dict-v1-cold',?3,0)")?
                                .execute(params![pid, raw, enc.compress(body)?])?;
                            db.prepare_cached("DELETE FROM packed_payloads WHERE id=?1")?.execute([pid])?;
                        }
                    }
                }
            }
        }
        for (id, pack, sha) in chunk_rows {
            db.execute("DELETE FROM cold_chunks WHERE id=?1", [id])?;
            db.execute("DELETE FROM cold_sources WHERE chunk_id=?1", [id])?;
            store::queue_gc(db, &pack, &store::chunk_key(&sha))?;
        }
        for (pack, sha) in db
            .prepare("SELECT pack,sha256 FROM logs WHERE session=?1")?
            .query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
        {
            store::queue_gc(db, &pack, &store::log_key(&sha))?;
        }
        for table in ["evidence", "audio_activity", "utterances", "transcript_events", "logs"] {
            db.execute(&format!("DELETE FROM {table} WHERE session=?1"), [session])?;
        }
        db.execute("DELETE FROM segments WHERE session=?1", [session])?;
        db.execute("DELETE FROM sessions WHERE id=?1", [session])?;
        // Objects the failed attempt catalogued that nothing references any more go to collection.
        // Migration holds the compactor's ownership lock; capture catalogues and references in one transaction.
        let orphans: Vec<(String, String)> = db
            .prepare("SELECT id,pack FROM audio_objects o WHERE NOT EXISTS(SELECT 1 FROM blocks WHERE object_id=o.id) AND NOT EXISTS(SELECT 1 FROM packed_audio_refs WHERE object_id=o.id) AND NOT EXISTS(SELECT 1 FROM clips WHERE object_id=o.id)")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, pack) in orphans {
            db.execute("DELETE FROM audio_objects WHERE id=?1", [&id])?;
            store::queue_gc(db, &pack, &store::audio_key(&id))?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => db.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(())
}
fn name_wall(name: &str, dir: &Path) -> u64 {
    name.strip_prefix("session-")
        .and_then(|s| s.split('-').next())
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(|ms| ms.saturating_mul(1_000_000))
        .or_else(|| {
            std::fs::metadata(dir)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64)
        })
        .unwrap_or(0)
}
fn mtime_ns(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
}
fn v_int(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(n) => Some(*n),
        Value::Real(f) => Some(*f as i64),
        Value::Text(s) => s.parse().ok(),
        _ => None,
    }
}
/// Rows of a legacy table as column-name maps (missing tables give no rows).
fn rows(c: &Container, table: &str, order: &str) -> Result<Vec<HashMap<String, Value>>> {
    if !c.has_table(table)? {
        return Ok(vec![]);
    }
    let mut stmt = c.db.prepare(&format!("SELECT * FROM {table} {order}"))?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut out = vec![];
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        let mut m = HashMap::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), r.get::<_, Value>(i)?);
        }
        out.push(m);
    }
    Ok(out)
}
fn get<'a>(m: &'a HashMap<String, Value>, k: &str) -> &'a Value {
    m.get(k).unwrap_or(&Value::Null)
}
fn text(m: &HashMap<String, Value>, k: &str) -> Option<String> {
    match get(m, k) {
        Value::Text(s) => Some(s.clone()),
        _ => None,
    }
}
fn int(m: &HashMap<String, Value>, k: &str) -> Option<i64> {
    v_int(get(m, k))
}
/// Runs `f` inside one index write transaction.
fn tx<T>(db: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let _raised = crate::live::Raised::new();
    db.execute_batch("BEGIN IMMEDIATE")?;
    match f() {
        Ok(v) => {
            db.execute_batch("COMMIT")?;
            Ok(v)
        }
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
/// Commits blobs to their packs, then catalogues them; batches bound memory and lock time.
struct Blobs<'a> {
    root: &'a Path,
    pending: Vec<(String, String, Vec<u8>)>,
    bytes: usize,
    count: u64,
    total: u64,
}
impl<'a> Blobs<'a> {
    fn push(&mut self, pack: String, key: String, data: Vec<u8>) -> Result<()> {
        self.bytes += data.len();
        self.count += 1;
        self.total += data.len() as u64;
        self.pending.push((pack, key, data));
        if self.bytes > 64 * 1024 * 1024 || self.pending.len() >= 512 {
            self.flush()?;
        }
        Ok(())
    }
    fn flush(&mut self) -> Result<()> {
        let mut by_pack: BTreeMap<String, Vec<(String, &[u8])>> = BTreeMap::new();
        for (pack, key, data) in &self.pending {
            by_pack.entry(pack.clone()).or_default().push((key.clone(), data.as_slice()));
        }
        for (pack, items) in by_pack {
            store::put(self.root, &pack, &items)?;
        }
        self.pending.clear();
        self.bytes = 0;
        Ok(())
    }
}

fn migrate_session(root: &Path, db: &Connection, dir: &Path, report: &mut Report) -> Result<()> {
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    purge_session(db, &name)?;
    let complete = dir.join("COMPLETE");
    let session = tx(db, || {
        db.execute(
            "INSERT INTO sessions(name,started_wall_ns,complete_wall_ns) VALUES(?1,?2,?3)",
            params![name, name_wall(&name, dir) as i64, complete.is_file().then(|| mtime_ns(&complete).unwrap_or(0) as i64)],
        )?;
        Ok(db.last_insert_rowid())
    })?;
    let mut legacy = Digest_::default();
    let mut blobs = Blobs { root, pending: vec![], bytes: 0, count: 0, total: 0 };
    let mut handled: HashSet<PathBuf> = HashSet::new();
    let mut objects: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut images: BTreeMap<String, (String, String, Vec<u8>)> = BTreeMap::new();
    let mut vectors: BTreeSet<String> = BTreeSet::new();
    // Capture containers.
    let mut captures: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p.file_name().unwrap().to_string_lossy();
            n.starts_with("session-") && (n.ends_with(".sqlite") || n.ends_with(".sqlite.zst"))
        })
        .collect();
    captures.sort();
    for path in &captures {
        handled.insert(path.clone());
        for side in ["-wal", "-shm"] {
            handled.insert(PathBuf::from(format!("{}{side}", path.display())));
        }
        let file = path.file_name().unwrap().to_string_lossy().to_string();
        if file.ends_with(".sqlite.zst") && dir.join(file.trim_end_matches(".zst")).is_file() {
            continue;
        }
        let segment_name = file.trim_end_matches(".zst").trim_end_matches(".sqlite").to_owned();
        let c = Container::open(path).with_context(|| format!("legacy capture {file}"))?;
        migrate_capture(root, db, dir, &c, session, &segment_name, &mut legacy, &mut blobs, &mut objects)?;
    }
    // Evidence journal.
    for file in ["evidence.sqlite", "evidence.sqlite.zst"] {
        handled.insert(dir.join(file));
    }
    for side in ["-wal", "-shm"] {
        handled.insert(dir.join(format!("evidence.sqlite{side}")));
    }
    let evidence = if dir.join("evidence.sqlite").is_file() {
        Some(dir.join("evidence.sqlite"))
    } else if dir.join("evidence.sqlite.zst").is_file() {
        Some(dir.join("evidence.sqlite.zst"))
    } else {
        None
    };
    if let Some(path) = evidence {
        let c = Container::open(&path).context("legacy evidence")?;
        migrate_evidence(root, db, dir, &c, session, &mut legacy, &mut blobs, &mut images, &mut vectors)?;
    }
    // Transcript projection.
    handled.insert(dir.join("transcript.sqlite"));
    for side in ["-wal", "-shm"] {
        handled.insert(dir.join(format!("transcript.sqlite{side}")));
    }
    if dir.join("transcript.sqlite").is_file() {
        let c = Container::open(&dir.join("transcript.sqlite")).context("legacy transcript")?;
        let utterances = rows(&c, "utterances", "ORDER BY rowid")?;
        let events = rows(&c, "events", "ORDER BY id")?;
        tx(db, || {
            for u in &utterances {
                let row = utterance_row(u);
                db.prepare_cached("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)")?
                    .execute(params![session, text(u, "id"), int(u, "wall_ms"), text(u, "source_id"), text(u, "source"), text(u, "speaker"), int(u, "start_ns"), int(u, "end_ns"), text(u, "text"), text(u, "words"), text(u, "notes"), text(u, "decision")])?;
                legacy.add("utterances", row);
            }
            for e in &events {
                db.prepare_cached("INSERT INTO transcript_events(session,kind,source_id,host_ns,payload) VALUES(?1,?2,?3,?4,?5)")?
                    .execute(params![session, text(e, "kind"), text(e, "source_id"), int(e, "host_ns"), text(e, "payload")])?;
                legacy.add("transcript_events", json!([text(e, "kind"), text(e, "source_id"), int(e, "host_ns"), text(e, "payload")]));
            }
            Ok(())
        })?;
    }
    blobs.flush()?;
    // Everything else that is not lock/marker state becomes an archived log.
    let mut files = vec![];
    collect_files(dir, dir, &mut files)?;
    let session_path = store::session_path(root, &name);
    for (relative, path) in files {
        if handled.contains(&path) || ignored(&relative) {
            continue;
        }
        let bytes = if relative.ends_with(".sqlite") {
            store::sqlite_bytes(&path)?
        } else {
            std::fs::read(&path)?
        };
        if bytes.is_empty() {
            continue;
        }
        let (log_name, content) = if let Some(rest) = relative.strip_prefix("logs/").and_then(|r| r.strip_suffix(".jsonl.zst")) {
            let content = crate::decompress_log(&bytes)?;
            if let Some(sha) = rest.strip_prefix("dry-run-") {
                ensure!(hash(&content) == sha, "rotated log {relative} checksum mismatch");
            }
            (format!("{rest}.jsonl"), content)
        } else {
            (relative.clone(), bytes)
        };
        store::archive_log(&session_path, &log_name, &content)?;
        legacy.add("logs", json!([log_name, hash(&content)]));
    }
    // Pack old rows now, so the check below covers the packed form.
    let cutoff = wall_ns().saturating_sub(crate::DAY_NS);
    let queue = crate::live::Queue::shared(root)?;
    while chunks::pack_blocks(db, &queue, root, cutoff)? > 0 {}
    while chunks::pack_evidence(db, &queue, root, cutoff)? > 0 {}
    // Rows that moved into chunks leave free index pages behind: return them now.
    let free: i64 = db.query_row("PRAGMA freelist_count", [], |r| r.get(0))?;
    crate::compact::reclaim_index(&queue, free.clamp(0, u32::MAX as i64) as u32, &mut || true)?;
    crate::compact::crash_point("migrate_copied");
    // Verify through the store's own readers.
    let mut store_side = Digest_::default();
    let view = crate::View::session_locked(&session_path)?;
    digest_store(&view, &mut store_side)?;
    for (id, expected) in &objects {
        let (rate, channels, frames, codec, bytes) = audio::object_bytes(&view.db, root, id)?;
        store_side.add("audio_objects", json!([id, rate, channels, frames, codec, hash(&bytes)]));
        let _ = expected;
    }
    for (id, (mime, digest, original)) in &images {
        // The same screenshot may already be catalogued by another session in its cold form:
        // then the pixels must match exactly.
        let (stored_mime, bytes) = crate::records::image_bytes(&view.db, root, id)?;
        let same = (stored_mime == *mime && hash(&bytes) == *digest)
            || image::load_from_memory(&bytes)?.to_rgba8() == image::load_from_memory(original)?.to_rgba8();
        store_side.add("images", if same { json!(id) } else { json!([id, "differs"]) });
    }
    for id in &vectors {
        let values = crate::records::vector_values(&view.db, id)?;
        ensure!(hash(&audio::pcm_bytes(&values)) == *id, "vector {id}");
        store_side.add("vectors", json!(id));
    }
    for log in store::logs(&session_path)? {
        let bytes = store::read_log(&session_path, &log)?;
        store_side.add("logs", json!([log, hash(&bytes)]));
    }
    drop(view);
    for (id, row) in &objects {
        let _ = id;
        legacy.add("audio_objects", row.clone());
    }
    for id in images.keys() {
        legacy.add("images", json!(id));
    }
    for id in &vectors {
        legacy.add("vectors", json!(id));
    }
    report.blobs = blobs.count;
    report.blob_bytes = blobs.total;
    for (t, tally) in &legacy.tables {
        report.rows.insert((*t).to_owned(), tally.count);
    }
    report.digest = legacy.combined();
    for (t, tally) in &legacy.tables {
        let other = store_side.tables.get(t).cloned().unwrap_or_default();
        ensure!(*tally == other, "table {t}: legacy {} != store {}", tally.hex(), other.hex());
    }
    for (t, tally) in &store_side.tables {
        ensure!(legacy.tables.contains_key(t) || tally.count == 0, "store has unexpected {t} rows");
    }
    Ok(())
}
use std::collections::BTreeSet;
fn utterance_row(u: &HashMap<String, Value>) -> serde_json::Value {
    json!([text(u, "id"), int(u, "wall_ms"), text(u, "source_id"), text(u, "source"), text(u, "speaker"), int(u, "start_ns"), int(u, "end_ns"), text(u, "text"), text(u, "words"), text(u, "notes"), text(u, "decision")])
}
fn ignored(relative: &str) -> bool {
    let base = relative.rsplit('/').next().unwrap_or(relative);
    matches!(base, ".compact.lock" | ".compact.gate" | ".audio-gc-wall" | ".DS_Store" | "COMPLETE")
        || base.ends_with(".part")
        || base.ends_with("-wal")
        || base.ends_with("-shm")
        || relative.starts_with("objects/")
        || relative.split('/').any(|c| c.starts_with(".seal-"))
}
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let t = e.file_type()?;
        let p = e.path();
        if t.is_dir() {
            collect_files(base, &p, out)?;
        } else if t.is_file() {
            out.push((p.strip_prefix(base)?.to_string_lossy().to_string(), p));
        }
    }
    Ok(())
}
/// A legacy SQLite file's consistent bytes, WAL included (online backup into memory).
fn check_sha(bytes: &[u8], digest: &str, what: &str) -> Result<()> {
    ensure!(hash(bytes) == digest, "{what} checksum mismatch");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn migrate_capture(
    root: &Path,
    db: &Connection,
    dir: &Path,
    c: &Container,
    session: i64,
    segment_name: &str,
    legacy: &mut Digest_,
    blobs: &mut Blobs,
    objects: &mut BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let head = rows(c, "session", "LIMIT 1")?;
    let head = head.first().context("legacy capture without a session row")?;
    let version = int(head, "version").unwrap_or(1);
    let wall_anchor = int(head, "wall_unix_ns").context("legacy wall anchor")?;
    let host_origin = int(head, "host_origin_ns").unwrap_or(0);
    let codec = text(head, "codec").unwrap_or_default();
    let segment = tx(db, || {
        db.execute(
            "INSERT INTO segments(session,name,version,wall_unix_ns,host_origin_ns,codec) VALUES(?1,?2,?3,?4,?5,?6)",
            params![session, segment_name, version, wall_anchor, host_origin, codec],
        )?;
        Ok(db.last_insert_rowid())
    })?;
    legacy.add("segments", json!([segment_name, version, wall_anchor, host_origin, codec]));
    let sources = rows(c, "sources", "ORDER BY id")?;
    let gaps = rows(c, "gaps", "ORDER BY id")?;
    tx(db, || {
        for s in &sources {
            db.execute(
                "INSERT INTO sources VALUES(?1,?2,?3,?4,?5)",
                params![segment, text(s, "id"), text(s, "metadata_json"), int(s, "added_ns"), int(s, "removed_ns")],
            )?;
            legacy.add("sources", json!([segment_name, text(s, "id"), text(s, "metadata_json"), int(s, "added_ns"), int(s, "removed_ns")]));
        }
        for g in &gaps {
            db.execute(
                "INSERT INTO gaps(segment,source_id,host_ns,detail) VALUES(?1,?2,?3,?4)",
                params![segment, text(g, "source_id"), int(g, "host_ns"), text(g, "detail")],
            )?;
            legacy.add("gaps", json!([segment_name, text(g, "source_id"), int(g, "host_ns"), text(g, "detail")]));
        }
        Ok(())
    })?;
    // Objects: inline hot bytes go to the hot pack, object files to the month of their audio.
    let has_objects = c.has_table("audio_objects")?;
    let pending_gc: HashSet<String> = if c.has_table("gc_objects")? {
        c.db.prepare("SELECT id FROM gc_objects")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    } else {
        HashSet::new()
    };
    let columns = c.columns("blocks")?;
    let has = |n: &str| columns.iter().any(|c| c == n);
    let wall_expr = if has("wall_ns") { format!("COALESCE(wall_ns,{wall_anchor}+host_ns-{host_origin})") } else { format!("{wall_anchor}+host_ns-{host_origin}") };
    let mut referenced: HashMap<String, i64> = HashMap::new();
    if has("object_id") {
        let mut stmt = c.db.prepare(&format!("SELECT object_id,min({wall_expr}) FROM blocks WHERE object_id IS NOT NULL GROUP BY object_id"))?;
        for r in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (id, w) = r?;
            referenced.insert(id, w);
        }
    }
    if c.has_table("packed_audio_refs")? {
        for id in c.db.prepare("SELECT object_id FROM packed_audio_refs")?.query_map([], |r| r.get::<_, String>(0))? {
            referenced.entry(id?).or_insert(wall_anchor);
        }
    }
    if c.has_table("audio_spans")? {
        for id in c.db.prepare("SELECT DISTINCT object_id FROM audio_spans")?.query_map([], |r| r.get::<_, String>(0))? {
            referenced.entry(id?).or_insert(wall_anchor);
        }
    }
    if has_objects {
        let mut stmt = c.db.prepare("SELECT id,rate,channels,frames,codec,path,stored_sha256,data FROM audio_objects ORDER BY id")?;
        let mut q = stmt.query([])?;
        let mut catalogue = vec![];
        while let Some(r) = q.next()? {
            let id: String = r.get(0)?;
            if pending_gc.contains(&id) && !referenced.contains_key(&id) {
                continue;
            }
            let (rate, channels, frames, codec, path, digest): (u32, u16, i64, String, String, String) =
                (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?);
            let data: Option<Vec<u8>> = r.get(7)?;
            let (bytes, pack) = match data {
                Some(b) => (b, store::HOT.to_owned()),
                None => {
                    let file = crate::object_path(dir, &path)?;
                    let b = std::fs::read(&file).with_context(|| format!("legacy object file {path}"))?;
                    (b, store::month(referenced.get(&id).copied().unwrap_or(wall_anchor).max(0) as u64))
                }
            };
            check_sha(&bytes, &digest, &format!("legacy audio object {id}"))?;
            let row = json!([id, rate, channels, frames, codec, digest]);
            objects.insert(id.clone(), row);
            blobs.push(pack.clone(), store::audio_key(&id), bytes)?;
            catalogue.push((id, rate, channels, frames, codec, pack, digest));
            if catalogue.len() >= 512 {
                blobs.flush()?;
                publish_objects(db, root, &mut catalogue)?;
            }
        }
        blobs.flush()?;
        publish_objects(db, root, &mut catalogue)?;
    }
    // Timeline rows, each source in time order: rows already cold and older than a day go straight
    // into chunks (no live-index round trip); the rest become live rows. Inline PCM becomes a hot object.
    let cutoff = wall_ns().saturating_sub(crate::DAY_NS) as i64;
    let mut stmt = c.db.prepare("SELECT * FROM blocks ORDER BY source_id,host_ns,id")?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    let mut q = stmt.query([])?;
    let mut batch: Vec<[Value; 11]> = vec![];
    let mut cold: Vec<[Value; 11]> = vec![];
    let mut inline_objects: Vec<audio::Encoded> = vec![];
    let flush_rows = |batch: &mut Vec<[Value; 11]>, inline: &mut Vec<audio::Encoded>| -> Result<()> {
        if batch.is_empty() && inline.is_empty() {
            return Ok(());
        }
        if !inline.is_empty() {
            let items: Vec<(String, &[u8])> = inline.iter().map(|o| (store::audio_key(&o.id), o.bytes.as_slice())).collect();
            store::put(root, store::HOT, &items)?;
        }
        tx(db, || {
            audio::publish(db, root, store::HOT, inline)?;
            let mut insert = db.prepare_cached("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,gain,wall_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)")?;
            for row in batch.iter() {
                let mut values = vec![Value::Integer(segment)];
                values.extend(row.iter().cloned());
                insert.execute(rusqlite::params_from_iter(values))?;
            }
            Ok(())
        })?;
        batch.clear();
        inline.clear();
        Ok(())
    };
    let flush_cold = |cold: &mut Vec<[Value; 11]>| -> Result<()> {
        if cold.is_empty() {
            return Ok(());
        }
        db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS mig_blocks AS SELECT * FROM main.blocks WHERE 0; DELETE FROM temp.mig_blocks;")?;
        let first = tx(db, || chunks::reserve_ids(db, "blocks", cold.len()))?;
        let mut wall = i64::MAX;
        {
            let mut insert = db.prepare_cached("INSERT INTO temp.mig_blocks(id,segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,gain,wall_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)")?;
            for (i, row) in cold.iter().enumerate() {
                if let Value::Integer(w) = row[10] {
                    wall = wall.min(w);
                }
                let mut values = vec![Value::Integer(first + i as i64), Value::Integer(segment)];
                values.extend(row.iter().cloned());
                insert.execute(rusqlite::params_from_iter(values))?;
            }
        }
        let raw = chunks::encode(db, &[("blocks", "SELECT * FROM temp.mig_blocks ORDER BY id")])?;
        let last = first + cold.len() as i64 - 1;
        chunks::publish_built(db, root, "blocks", segment, (first, last), &raw, wall.max(0) as u64, |_| {
            db.execute("INSERT OR IGNORE INTO packed_audio_refs SELECT DISTINCT object_id FROM temp.mig_blocks WHERE object_id IS NOT NULL", [])?;
            Ok(())
        })?;
        cold.clear();
        Ok(())
    };
    while let Some(r) = q.next()? {
        let mut m = HashMap::new();
        for (i, n) in names.iter().enumerate() {
            m.insert(n.clone(), r.get::<_, Value>(i)?);
        }
        let host = int(&m, "host_ns").context("block host_ns")?;
        let rate = int(&m, "rate").context("block rate")?;
        let channels = int(&m, "channels").context("block channels")?;
        let frames = int(&m, "frames").context("block frames")?;
        let codec = text(&m, "codec").context("block codec")?;
        let discontinuity = int(&m, "discontinuity").unwrap_or(0);
        let wall = int(&m, "wall_ns").unwrap_or(wall_anchor + host - host_origin);
        let gain = match get(&m, "gain") {
            Value::Real(g) => *g,
            Value::Integer(g) => *g as f64,
            _ => 1.,
        };
        let mut object = text(&m, "object_id");
        let offset = int(&m, "offset_frames").unwrap_or(0);
        if object.is_none() && codec == "f32-zstd" {
            let Value::Blob(pcm) = get(&m, "pcm") else { anyhow::bail!("legacy inline block without PCM") };
            let n = (frames * channels) as usize;
            let samples = audio::pcm_from_bytes(&zstd::bulk::decompress(pcm, n * 4)?)?;
            ensure!(samples.len() == n, "legacy inline PCM length");
            let encoded = audio::encode_lossless(rate as u32, channels as u16, &samples)?;
            object = Some(encoded.id.clone());
            objects.insert(encoded.id.clone(), json!([encoded.id, rate, channels, frames, encoded.codec, encoded.stored_sha256]));
            blobs.count += 1;
            blobs.total += encoded.bytes.len() as u64;
            inline_objects.push(encoded);
        }
        legacy.add(
            "blocks",
            json!([segment_name, text(&m, "source_id"), host, rate, channels, frames, codec, discontinuity, object, offset, gain, wall]),
        );
        let packable = (codec == "cold-ref" || codec == "zero") && rate > 0 && wall + frames * 1_000_000_000 / rate <= cutoff;
        let row = [
            get(&m, "source_id").clone(),
            Value::Integer(host),
            Value::Integer(rate),
            Value::Integer(channels),
            Value::Integer(frames),
            Value::Text(codec),
            Value::Integer(discontinuity),
            object.map_or(Value::Null, Value::Text),
            Value::Integer(offset),
            Value::Real(gain),
            Value::Integer(wall),
        ];
        // Each class switch flushes the other buffer, so new IDs keep every source's time order.
        if packable {
            flush_rows(&mut batch, &mut inline_objects)?;
            cold.push(row);
            if cold.len() as i64 >= chunks::CHUNK_ROWS {
                flush_cold(&mut cold)?;
            }
        } else {
            flush_cold(&mut cold)?;
            batch.push(row);
            if batch.len() >= 8192 || inline_objects.len() >= 256 {
                flush_rows(&mut batch, &mut inline_objects)?;
            }
        }
    }
    flush_cold(&mut cold)?;
    flush_rows(&mut batch, &mut inline_objects)?;
    // Deduplication index and packed-object references.
    let spans = rows(c, "audio_spans", "ORDER BY rowid")?;
    let packed = rows(c, "packed_audio_refs", "")?;
    tx(db, || {
        for s in &spans {
            db.execute(
                "INSERT INTO audio_spans VALUES(?1,?2,?3,?4,?5,?6)",
                params![text(s, "source"), int(s, "start_ns"), int(s, "end_ns"), text(s, "object_id"), int(s, "quality"), text(s, "fingerprint")],
            )?;
        }
        for p in &packed {
            db.execute("INSERT OR IGNORE INTO packed_audio_refs VALUES(?1)", [text(p, "object_id")])?;
        }
        Ok(())
    })?;
    Ok(())
}
#[allow(clippy::type_complexity)]
fn publish_objects(db: &Connection, root: &Path, catalogue: &mut Vec<(String, u32, u16, i64, String, String, String)>) -> Result<()> {
    if catalogue.is_empty() {
        return Ok(());
    }
    tx(db, || {
        for (id, rate, channels, frames, codec, pack, digest) in catalogue.iter() {
            let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM audio_objects WHERE id=?1)", [id], |r| r.get(0))?;
            if exists {
                continue;
            }
            ensure!(store::has(root, pack, &store::audio_key(id))?, "migrated object {id} missing from pack {pack}");
            db.execute(
                "INSERT INTO audio_objects VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![id, rate, channels, frames, codec, pack, digest],
            )?;
        }
        Ok(())
    })?;
    catalogue.clear();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn migrate_evidence(
    root: &Path,
    db: &Connection,
    dir: &Path,
    c: &Container,
    session: i64,
    legacy: &mut Digest_,
    blobs: &mut Blobs,
    images: &mut BTreeMap<String, (String, String, Vec<u8>)>,
    vectors: &mut BTreeSet<String>,
) -> Result<()> {
    // Rows older than a day go straight into chunks with the bodies only they use; the rest become
    // live rows with their bodies. Every body is verified against its content identity.
    let cutoff = wall_ns().saturating_sub(crate::DAY_NS) as i64;
    let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
    let mut enc = zstd::bulk::Compressor::with_dictionary(1, DICTIONARY)?;
    if c.has_table("evidence")? {
        ensure!(c.has_table("payloads")?, "legacy evidence without payloads");
        db.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS mig_refs(id TEXT PRIMARY KEY, n INTEGER NOT NULL); DELETE FROM temp.mig_refs;
             CREATE TEMP TABLE IF NOT EXISTS mig_evidence AS SELECT * FROM main.evidence WHERE 0; DELETE FROM temp.mig_evidence;
             CREATE TEMP TABLE IF NOT EXISTS mig_payloads(id TEXT PRIMARY KEY, raw_bytes INTEGER, codec TEXT, data BLOB); DELETE FROM temp.mig_payloads;",
        )?;
        // How many of this session's rows use each body: a timed VAD body nothing else needs keeps no row.
        db.execute_batch("BEGIN")?;
        {
            let mut count = db.prepare("INSERT INTO temp.mig_refs VALUES(?1,1) ON CONFLICT(id) DO UPDATE SET n=n+1")?;
            let mut stmt = c.db.prepare("SELECT payload_id FROM evidence")?;
            let mut q = stmt.query([])?;
            while let Some(r) = q.next()? {
                count.execute([r.get::<_, String>(0)?])?;
            }
        }
        db.execute_batch("COMMIT")?;
        let mut body_stmt = c.db.prepare("SELECT raw_bytes,codec,data FROM payloads WHERE id=?1 LIMIT 1")?;
        let mut body_of = |id: &str| -> Result<(i64, Vec<u8>)> {
            let (raw, codec, data): (i64, String, Vec<u8>) = body_stmt
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .with_context(|| format!("legacy evidence references payload {id}, which has no body"))?;
            let body = match codec.as_str() {
                "json-raw-chunk-v1" => data,
                "json-zstd-dict-v1" | "json-zstd-dict-v1-cold" => dec.decompress(&data, raw as usize)?,
                other => anyhow::bail!("legacy payload {id} codec {other}"),
            };
            ensure!(hash(&body) == id, "legacy payload {id} checksum mismatch");
            Ok((raw, body))
        };
        let known = |id: &str| -> Result<bool> {
            Ok(db
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM main.payloads WHERE id=?1) OR EXISTS(SELECT 1 FROM main.packed_payloads WHERE id=?1) OR EXISTS(SELECT 1 FROM temp.mig_payloads WHERE id=?1)")?
                .query_row([id], |r| r.get(0))?)
        };
        let release = |id: &str| -> Result<()> {
            db.prepare_cached("UPDATE temp.mig_refs SET n=n-1 WHERE id=?1")?.execute([id])?;
            Ok(())
        };
        type Row = (String, String, String, i64, i64, String);
        let mut live: Vec<Row> = vec![];
        let mut live_bodies: Vec<(String, i64, Vec<u8>)> = vec![];
        let mut live_ids: HashSet<String> = HashSet::new();
        let mut cold: Vec<Row> = vec![];
        let mut cold_bytes = 0i64;
        let flush_live = |live: &mut Vec<Row>, bodies: &mut Vec<(String, i64, Vec<u8>)>| -> Result<()> {
            if live.is_empty() {
                return Ok(());
            }
            tx(db, || {
                for (id, raw, data) in bodies.iter() {
                    db.prepare_cached("INSERT OR IGNORE INTO main.payloads VALUES(?1,?2,'json-zstd-dict-v1',?3,NULL)")?.execute(params![id, raw, data])?;
                }
                let mut ins = db.prepare_cached("INSERT INTO main.evidence(session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
                for (kind, source, ids, host, wall, payload) in live.iter() {
                    ins.execute(params![session, kind, source, ids, host, wall, payload])?;
                }
                Ok(())
            })?;
            live.clear();
            bodies.clear();
            Ok(())
        };
        let flush_cold = |cold: &mut Vec<Row>, cold_bytes: &mut i64| -> Result<()> {
            if cold.is_empty() {
                return Ok(());
            }
            let first = tx(db, || chunks::reserve_ids(db, "evidence", cold.len()))?;
            let mut wall = i64::MAX;
            {
                let mut ins = db.prepare_cached("INSERT INTO temp.mig_evidence(id,session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?;
                for (i, (kind, source, ids, host, w, payload)) in cold.iter().enumerate() {
                    wall = wall.min(*w);
                    ins.execute(params![first + i as i64, session, kind, source, ids, host, w, payload])?;
                }
            }
            let raw = chunks::encode(db, &[("payloads", "SELECT id,raw_bytes,codec,data FROM temp.mig_payloads ORDER BY id"), ("evidence", "SELECT * FROM temp.mig_evidence ORDER BY id")])?;
            let moved: Vec<(String, i64, i64, Option<(String, u64)>)> = db
                .prepare("SELECT p.id,p.raw_bytes,COALESCE(r.n,0),p.data FROM temp.mig_payloads p LEFT JOIN temp.mig_refs r ON r.id=p.id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, chunks::silero_start(&r.get::<_, Vec<u8>>(3)?))))?
                .collect::<rusqlite::Result<_>>()?;
            let last = first + cold.len() as i64 - 1;
            chunks::publish_built(db, root, "evidence", session, (first, last), &raw, wall.max(0) as u64, |chunk_id| {
                let mut ranges = BTreeMap::<String, (u64, u64)>::new();
                for (id, raw, remaining, timed) in &moved {
                    if let Some((source, start)) = timed {
                        let range = ranges.entry(source.clone()).or_insert((*start, *start));
                        range.0 = range.0.min(*start);
                        range.1 = range.1.max(*start);
                    }
                    // Other rows still to come (or held elsewhere) find the body through its placeholder.
                    if timed.is_none() || *remaining > 0 {
                        db.prepare_cached("INSERT OR IGNORE INTO main.packed_payloads VALUES(?1,?2,?3)")?.execute(params![id, raw, chunk_id])?;
                    }
                }
                for (source, (a, b)) in ranges {
                    db.execute("INSERT OR IGNORE INTO cold_sources VALUES(?1,?2,?3,?4)", params![chunk_id, source, a as i64, b as i64])?;
                }
                // Bodies these rows use that are held live elsewhere stay pinned.
                db.execute(
                    "UPDATE main.payloads SET chunk=0 WHERE chunk IS NULL AND codec IN ('json-zstd-dict-v1','json-zstd-dict-v1-cold') AND id IN (SELECT payload_id FROM temp.mig_evidence)",
                    [],
                )?;
                Ok(())
            })?;
            db.execute_batch("DELETE FROM temp.mig_evidence; DELETE FROM temp.mig_payloads;")?;
            cold.clear();
            *cold_bytes = 0;
            Ok(())
        };
        let mut stmt = c.db.prepare("SELECT kind,source,utterance_ids,host_ns,wall_ns,payload_id FROM evidence ORDER BY id")?;
        let mut q = stmt.query([])?;
        while let Some(r) = q.next()? {
            let row: Row = (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?);
            legacy.add("evidence", json!([row.0, row.1, row.2, row.3, row.4, row.5]));
            let payload = row.5.clone();
            if row.4 <= cutoff {
                flush_live(&mut live, &mut live_bodies)?;
                live_ids.clear();
                if !known(&payload)? {
                    let (raw, body) = body_of(&payload)?;
                    if !cold.is_empty() && cold_bytes + raw > chunks::PAYLOAD_BUDGET {
                        flush_cold(&mut cold, &mut cold_bytes)?;
                    }
                    // After a flush the body may now have a placeholder; it is then referenced, not repeated.
                    if !known(&payload)? {
                        db.prepare_cached("INSERT INTO temp.mig_payloads VALUES(?1,?2,'json-raw-chunk-v1',?3)")?.execute(params![payload, raw, body])?;
                        cold_bytes += raw;
                    }
                }
                cold.push(row);
                release(&payload)?;
                if cold.len() as i64 >= chunks::CHUNK_ROWS {
                    flush_cold(&mut cold, &mut cold_bytes)?;
                }
            } else {
                flush_cold(&mut cold, &mut cold_bytes)?;
                if !live_ids.contains(&payload) && !known(&payload)? {
                    let (raw, body) = body_of(&payload)?;
                    live_bodies.push((payload.clone(), raw, enc.compress(&body)?));
                    live_ids.insert(payload.clone());
                }
                live.push(row);
                release(&payload)?;
                if live.len() >= 8192 {
                    flush_live(&mut live, &mut live_bodies)?;
                    live_ids.clear();
                }
            }
        }
        flush_cold(&mut cold, &mut cold_bytes)?;
        flush_live(&mut live, &mut live_bodies)?;
    }
    if c.has_table("vectors")? {
        let rows: Vec<(String, i64, Vec<u8>)> = c
            .db
            .prepare("SELECT id,dimensions,data FROM vectors")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        tx(db, || {
            for (id, n, data) in &rows {
                let bytes = zstd::bulk::decompress(data, *n as usize * 4)?;
                ensure!(hash(&bytes) == *id, "legacy vector {id} checksum mismatch");
                db.execute("INSERT OR IGNORE INTO vectors VALUES(?1,?2,?3)", params![id, n, data])?;
                vectors.insert(id.clone());
            }
            Ok(())
        })?;
    }
    if c.has_table("images")? {
        let garbage: HashSet<String> = if c.has_table("garbage_images")? {
            c.db.prepare("SELECT path FROM garbage_images")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        } else {
            HashSet::new()
        };
        let cols = c.columns("images")?;
        let has_checked = cols.iter().any(|c| c == "cold_checked");
        let rows: Vec<(String, String, String, String, i64, i64)> = c
            .db
            .prepare(&format!("SELECT id,mime,path,stored_sha256,wall_ns,{} FROM images ORDER BY id", if has_checked { "cold_checked" } else { "0" }))?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut catalogue = vec![];
        for (id, mime, path, digest, wall, checked) in rows {
            let _ = &garbage;
            let bytes = std::fs::read(crate::object_path(dir, &path)?).with_context(|| format!("legacy image file {path}"))?;
            check_sha(&bytes, &digest, &format!("legacy image {id}"))?;
            let pack = if checked != 0 { store::month(wall.max(0) as u64) } else { store::HOT.to_owned() };
            let key = store::image_key(&id, &mime)?;
            images.insert(id.clone(), (mime.clone(), digest.clone(), bytes.clone()));
            blobs.push(pack.clone(), key.clone(), bytes)?;
            catalogue.push((id, mime, pack, digest, wall, checked, key));
        }
        blobs.flush()?;
        tx(db, || {
            for (id, mime, pack, digest, wall, checked, key) in &catalogue {
                let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM images WHERE id=?1)", [id], |r| r.get(0))?;
                if exists {
                    // The same screenshot is already catalogued (another session); keep that copy and
                    // let the collector drop this one.
                    store::queue_gc(db, pack, key)?;
                    continue;
                }
                ensure!(store::has(root, pack, key)?, "migrated image {id} missing");
                db.execute("INSERT INTO images VALUES(?1,?2,?3,?4,?5,?6)", params![id, mime, pack, digest, wall, checked])?;
            }
            Ok(())
        })?;
    }
    let activity = rows(c, "audio_activity", "ORDER BY source,start_ns")?;
    tx(db, || {
        for a in &activity {
            db.execute(
                "INSERT OR REPLACE INTO audio_activity VALUES(?1,?2,?3,?4,?5)",
                params![session, text(a, "source"), int(a, "start_ns"), int(a, "end_ns"), int(a, "speech")],
            )?;
            legacy.add("audio_activity", json!([text(a, "source"), int(a, "start_ns"), int(a, "end_ns"), int(a, "speech")]));
        }
        Ok(())
    })?;
    Ok(())
}
/// The store side of the verification digest, read through the session view.
fn digest_store(view: &crate::View, d: &mut Digest_) -> Result<()> {
    view.capture()?;
    view.evidence()?;
    view.transcript()?;
    let db = &view.db;
    let segment_names: HashMap<i64, String> = view.segments()?.into_iter().collect();
    for (id, name) in &segment_names {
        let row: (i64, i64, i64, String) = db.query_row(
            "SELECT version,wall_unix_ns,host_origin_ns,codec FROM main.segments WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        d.add("segments", json!([name, row.0, row.1, row.2, row.3]));
    }
    let seg = |id: i64| segment_names.get(&id).cloned().unwrap_or_default();
    let mut stmt = db.prepare("SELECT segment,id,metadata_json,added_ns,removed_ns FROM temp.sources")?;
    for r in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?)))? {
        let r = r?;
        d.add("sources", json!([seg(r.0), r.1, r.2, r.3, r.4]));
    }
    let mut stmt = db.prepare("SELECT segment,source_id,host_ns,detail FROM temp.gaps")?;
    for r in stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?)))? {
        let r = r?;
        d.add("gaps", json!([seg(r.0), r.1, r.2, r.3]));
    }
    let mut stmt = db.prepare("SELECT segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,gain,wall_ns FROM temp.blocks")?;
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        d.add(
            "blocks",
            json!([seg(r.get(0)?), r.get::<_, Option<String>>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, i64>(5)?, r.get::<_, String>(6)?, r.get::<_, i64>(7)?, r.get::<_, Option<String>>(8)?, r.get::<_, i64>(9)?, r.get::<_, f64>(10)?, r.get::<_, i64>(11)?]),
        );
    }
    let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
    let mut stmt = db.prepare("SELECT e.kind,e.source,e.utterance_ids,e.host_ns,e.wall_ns,e.payload_id,p.raw_bytes,p.codec,p.data FROM temp.evidence e LEFT JOIN temp.payloads p ON p.id=e.payload_id")?;
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        let id: String = r.get(5)?;
        let codec: Option<String> = r.get(7)?;
        let codec = codec.with_context(|| format!("evidence payload {id} has no body"))?;
        let body = crate::records::payload_body(db, view.root(), &id, r.get::<_, i64>(6)? as usize, &codec, r.get(8)?, &mut dec)?;
        ensure!(hash(&body) == id, "store payload {id} checksum mismatch");
        d.add("evidence", json!([r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, id]));
    }
    let mut stmt = db.prepare("SELECT id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision FROM temp.utterances")?;
    let mut q = stmt.query([])?;
    while let Some(r) = q.next()? {
        let mut m = HashMap::new();
        for (i, n) in ["id", "wall_ms", "source_id", "source", "speaker", "start_ns", "end_ns", "text", "words", "notes", "decision"].iter().enumerate() {
            m.insert((*n).to_owned(), r.get::<_, Value>(i)?);
        }
        d.add("utterances", utterance_row(&m));
    }
    let mut stmt = db.prepare("SELECT kind,source_id,host_ns,payload FROM main.transcript_events WHERE session=?1")?;
    for r in stmt.query_map([view.session_id()], |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?)))? {
        let r = r?;
        d.add("transcript_events", json!([r.0, r.1, r.2, r.3]));
    }
    let mut stmt = db.prepare("SELECT source,start_ns,end_ns,speech FROM main.audio_activity WHERE session=?1")?;
    for r in stmt.query_map([view.session_id()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))? {
        let r = r?;
        d.add("audio_activity", json!([r.0, r.1, r.2, r.3]));
    }
    Ok(())
}

/// The legacy voice-bank store: clips keep their IDs when free, audio moves to a month pack, and
/// an edited `lines.json` moves beside the archive as `voice-bank-lines.json`.
fn migrate_bank(root: &Path, db: &Connection, dir: &Path, report: &mut Report) -> Result<()> {
    let mut legacy = Digest_::default();
    let mut store_side = Digest_::default();
    let path = dir.join("voice-bank.sqlite");
    let mut moved_clips: Vec<i64> = vec![];
    if path.is_file() {
        let c = Container::open(&path).context("legacy voice bank")?;
        let pack = store::month(wall_ns());
        let mut objects = vec![];
        if c.has_table("audio_objects")? {
            #[allow(clippy::type_complexity)]
            let rows: Vec<(String, u32, u16, i64, String, String, Option<Vec<u8>>)> = c
                .db
                .prepare("SELECT id,rate,channels,frames,codec,stored_sha256,data FROM audio_objects ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, rate, channels, frames, codec, digest, data) in rows {
                let data = data.with_context(|| format!("voice bank object {id} has no bytes"))?;
                check_sha(&data, &digest, &format!("voice bank object {id}"))?;
                store::put(root, &pack, &[(store::audio_key(&id), &data)])?;
                report.blobs += 1;
                report.blob_bytes += data.len() as u64;
                legacy.add("audio_objects", json!([id, rate, channels, frames, codec, digest]));
                objects.push((id, rate, channels, frames, codec, pack.clone(), digest));
            }
        }
        publish_objects(db, root, &mut objects.clone())?;
        let clips = rows(&c, "clips", "ORDER BY id")?;
        tx(db, || {
            for k in &clips {
                let row = json!([text(k, "object_id"), text(k, "origin"), text(k, "line_id"), text(k, "text"), text(k, "language"), text(k, "voice"), text(k, "generator"), text(k, "settings"), int(k, "sample_rate"), int(k, "samples"), int(k, "created_wall_ns")]);
                legacy.add("clips", row);
                let free: bool = db.query_row("SELECT NOT EXISTS(SELECT 1 FROM clips WHERE id=?1)", [int(k, "id")], |r| r.get(0))?;
                let n = db.execute(
                    "INSERT OR IGNORE INTO clips(id,object_id,origin,line_id,text,language,voice,generator,settings,sample_rate,samples,created_wall_ns) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    params![if free { int(k, "id") } else { None }, text(k, "object_id"), text(k, "origin"), text(k, "line_id"), text(k, "text"), text(k, "language"), text(k, "voice"), text(k, "generator"), text(k, "settings"), int(k, "sample_rate"), int(k, "samples"), int(k, "created_wall_ns")],
                )?;
                if n == 1 {
                    moved_clips.push(db.last_insert_rowid());
                }
            }
            Ok(())
        })?;
        for (id, ..) in &objects {
            let (rate, channels, frames, codec, bytes) = audio::object_bytes(db, root, id)?;
            store_side.add("audio_objects", json!([id, rate, channels, frames, codec, hash(&bytes)]));
        }
        let mut stmt = db.prepare("SELECT object_id,origin,line_id,text,language,voice,generator,settings,sample_rate,samples,created_wall_ns FROM clips WHERE object_id=?1 AND origin=?2 AND ifnull(line_id,'')=ifnull(?3,'') AND voice=?4 AND generator=?5")?;
        for k in &clips {
            let r = stmt.query_row(params![text(k, "object_id"), text(k, "origin"), text(k, "line_id"), text(k, "voice"), text(k, "generator")], |r| {
                Ok(json!([r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?, r.get::<_, String>(7)?, r.get::<_, i64>(8)?, r.get::<_, i64>(9)?, r.get::<_, i64>(10)?]))
            })?;
            store_side.add("clips", r);
        }
    }
    let lines = dir.join("lines.json");
    if lines.is_file() {
        let target = root.parent().context("archive parent")?.join("voice-bank-lines.json");
        let bytes = std::fs::read(&lines)?;
        if !target.exists() {
            crate::atomic_write(&target, &bytes)?;
        }
        legacy.add("lines", json!(hash(&bytes)));
        let kept = std::fs::read(&target)?;
        store_side.add("lines", json!(hash(&kept)));
    }
    for (t, tally) in &legacy.tables {
        report.rows.insert((*t).to_owned(), tally.count);
        ensure!(Some(tally) == store_side.tables.get(t), "voice bank table {t} conflicts with destination after migration; legacy folder kept");
    }
    report.digest = legacy.combined();
    let _ = moved_clips;
    Ok(())
}
