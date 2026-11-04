//! Immutable cold table chunks (HAC3): rows of one capture segment or one session, dictionary
//! zstd19, stored in a month pack. A chunk is built and compressed outside the index write lock and
//! published in one short transaction that also removes the rows it holds.
use crate::{hash, records::DICTIONARY, store};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use std::{collections::HashMap, path::Path};

/// Rows a chunk may hold (decode's bound, and the timeline backfill's window).
pub const CHUNK_ROWS: i64 = 8192;
/// Rows a new chunk holds at most: its publication, one writer job, deletes that many rows (8,192 held the index's
/// write lock 0.2 s on an idle disk and minutes under throttled I/O).
pub const PACK_ROWS: i64 = 1024;
/// A candidate batch above this raw payload size packs one record alone.
pub const PAYLOAD_BUDGET: i64 = 16 * 1024 * 1024;
const DECODE_BOUND: usize = 128 * 1024 * 1024;

fn write_u32(out: &mut Vec<u8>, n: usize) {
    out.extend((n as u32).to_le_bytes());
}
fn blob(out: &mut Vec<u8>, b: &[u8]) {
    write_u32(out, b.len());
    out.extend(b);
}
/// One table stream: name, column names, row count, tagged values. Payload identities are
/// derived from their bodies (tag 5) and evidence references a payload in the same chunk by
/// ordinal (tag 6).
fn table(db: &Connection, out: &mut Vec<u8>, name: &str, query: &str, ids: &HashMap<String, usize>) -> Result<usize> {
    blob(out, name.as_bytes());
    let mut stmt = db.prepare(query)?;
    let names: Vec<String> = stmt.column_names().into_iter().map(str::to_owned).collect();
    write_u32(out, names.len());
    for n in &names {
        blob(out, n.as_bytes());
    }
    let payload_col = names.iter().position(|n| n == "payload_id");
    let mark = out.len();
    write_u32(out, 0);
    let mut count = 0;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        for i in 0..names.len() {
            if name == "payloads" && i == 0 {
                out.push(5);
                continue;
            }
            if name == "evidence" && Some(i) == payload_col {
                let id: String = row.get(i)?;
                if let Some(index) = ids.get(&id) {
                    out.push(6);
                    write_u32(out, *index);
                    continue;
                }
            }
            match row.get::<_, Value>(i)? {
                Value::Null => out.push(0),
                Value::Integer(n) => {
                    out.push(1);
                    out.extend(n.to_le_bytes());
                }
                Value::Real(n) => {
                    out.push(2);
                    out.extend(n.to_le_bytes());
                }
                Value::Text(s) => {
                    out.push(3);
                    blob(out, s.as_bytes());
                }
                Value::Blob(b) => {
                    out.push(4);
                    blob(out, &b);
                }
            }
        }
        count += 1;
    }
    out[mark..mark + 4].copy_from_slice(&(count as u32).to_le_bytes());
    Ok(count)
}

/// Decoded chunk: table name → (column names, rows).
pub type Tables = Vec<(String, Vec<String>, Vec<Vec<Value>>)>;
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        ensure!(n <= self.bytes.len().saturating_sub(self.pos), "truncated chunk");
        let b = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(b)
    }
    fn u32(&mut self) -> Result<usize> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into()?) as usize)
    }
    fn blob(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()?;
        self.take(n)
    }
}
pub fn decode(raw: &[u8]) -> Result<Tables> {
    let mut r = Reader { bytes: raw, pos: 0 };
    ensure!(r.take(4)? == b"HAC3", "chunk version");
    let mut out: Tables = vec![];
    let mut ids: Vec<String> = vec![];
    while r.pos < raw.len() {
        let name = std::str::from_utf8(r.blob()?)?.to_owned();
        ensure!(matches!(name.as_str(), "blocks" | "evidence" | "payloads"), "chunk table");
        let cols = r.u32()?;
        ensure!(cols <= 32, "chunk geometry");
        let mut names = vec![];
        for _ in 0..cols {
            names.push(std::str::from_utf8(r.blob()?)?.to_owned());
        }
        let count = r.u32()?;
        ensure!(count as i64 <= CHUNK_ROWS, "chunk geometry");
        let data_col = names.iter().position(|n| n == "data");
        let mut rows = Vec::with_capacity(count);
        for _ in 0..count {
            let mut values = Vec::with_capacity(cols);
            let mut derived = false;
            for col in 0..cols {
                let tag = r.take(1)?[0];
                values.push(match tag {
                    0 => Value::Null,
                    1 => Value::Integer(i64::from_le_bytes(r.take(8)?.try_into()?)),
                    2 => Value::Real(f64::from_le_bytes(r.take(8)?.try_into()?)),
                    3 => Value::Text(std::str::from_utf8(r.blob()?)?.to_owned()),
                    4 => Value::Blob(r.blob()?.to_vec()),
                    5 => {
                        ensure!(col == 0, "derived identity column");
                        derived = true;
                        Value::Null
                    }
                    6 => Value::Text(ids.get(r.u32()?).context("payload ordinal")?.clone()),
                    _ => anyhow::bail!("chunk value"),
                });
            }
            if derived {
                let Some(Value::Blob(bytes)) = data_col.and_then(|i| values.get(i)) else {
                    anyhow::bail!("derived payload body")
                };
                values[0] = Value::Text(hash(bytes));
            }
            if name == "payloads" {
                let Value::Text(id) = &values[0] else { anyhow::bail!("payload identity") };
                ids.push(id.clone());
            }
            rows.push(values);
        }
        out.push((name, names, rows));
    }
    Ok(out)
}
/// Reads, verifies and decodes one catalogued chunk.
pub fn load(db: &Connection, root: &Path, chunk_id: i64) -> Result<Tables> {
    let (pack, raw_bytes, sha): (String, i64, String) = db
        .prepare_cached("SELECT pack,raw_bytes,sha256 FROM main.cold_chunks WHERE id=?1")?
        .query_row([chunk_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    ensure!((0..=DECODE_BOUND as i64).contains(&raw_bytes), "chunk bound");
    let data = store::get(root, &pack, &store::chunk_key(&sha))?
        .with_context(|| format!("chunk {chunk_id} missing from pack {pack}"))?;
    let raw = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?.decompress(&data, raw_bytes as usize)?;
    ensure!(hash(&raw) == sha, "chunk checksum");
    decode(&raw)
}
/// Inserts a decoded chunk's rows of `table` into `target`, matching columns by name.
pub fn insert_rows(db: &Connection, tables: &Tables, table_name: &str, target: &str) -> Result<usize> {
    let target_cols: Vec<String> = db
        .prepare(&format!("SELECT * FROM {target} LIMIT 0"))?
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut n = 0;
    for (name, cols, rows) in tables {
        if name != table_name {
            continue;
        }
        let map: Vec<Option<usize>> = target_cols.iter().map(|c| cols.iter().position(|x| x == c)).collect();
        let sql = format!(
            "INSERT OR IGNORE INTO {target}({}) VALUES({})",
            target_cols.join(","),
            vec!["?"; target_cols.len()].join(",")
        );
        let mut insert = db.prepare_cached(&sql)?;
        for row in rows {
            let values = map.iter().map(|i| i.map_or(Value::Null, |i| row[i].clone()));
            insert.execute(rusqlite::params_from_iter(values))?;
            n += 1;
        }
    }
    Ok(n)
}
fn compress(raw: &[u8]) -> Result<Vec<u8>> {
    ensure!(raw.len() <= DECODE_BOUND, "metadata chunk too large");
    let data = zstd::bulk::Compressor::with_dictionary(19, DICTIONARY)?.compress(raw)?;
    ensure!(
        zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?.decompress(&data, raw.len())? == raw,
        "chunk round trip"
    );
    Ok(data)
}
fn temp_ids(db: &Connection, ids: &[i64]) -> Result<()> {
    db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS pack_ids(id INTEGER PRIMARY KEY); DELETE FROM temp.pack_ids;")?;
    let mut insert = db.prepare_cached("INSERT INTO temp.pack_ids VALUES(?1)")?;
    for id in ids {
        insert.execute([id])?;
    }
    Ok(())
}
/// Compresses, verifies and commits a chunk blob to its month pack before any reference exists.
fn publish_chunk(root: &Path, raw: &[u8], wall: u64) -> Result<(String, String, Vec<u8>)> {
    let data = compress(raw)?;
    let sha = hash(raw);
    let pack = store::month(wall);
    store::put(root, &pack, &[(store::chunk_key(&sha), &data)])?;
    crate::compact::crash_point("chunk_verified");
    Ok((pack, sha, data))
}

/// HAC3 stream of the given `(table, query)` pairs; payload ordinals as in `table`.
pub(crate) fn encode(db: &Connection, parts: &[(&str, &str)]) -> Result<Vec<u8>> {
    let mut raw = b"HAC3".to_vec();
    let mut ordinals = HashMap::new();
    for (name, query) in parts {
        if *name == "payloads" {
            let ids: Vec<String> = db.prepare(query)?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            ordinals = ids.into_iter().enumerate().map(|(i, id)| (id, i)).collect();
        }
        table(db, &mut raw, name, query, &ordinals)?;
    }
    Ok(raw)
}
/// Publishes a chunk built elsewhere (migration): blob first, then one index transaction that
/// records it and runs `extra(chunk_id)`. Returns the chunk ID.
pub(crate) fn publish_built(
    db: &Connection,
    root: &Path,
    kind: &str,
    scope: i64,
    ids: (i64, i64),
    raw: &[u8],
    wall: u64,
    extra: impl FnOnce(i64) -> Result<()>,
) -> Result<i64> {
    let (pack, sha, data) = publish_chunk(root, raw, wall)?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| -> Result<i64> {
        store::ensure_blob(root, &pack, &store::chunk_key(&sha), &data)?;
        db.execute(
            "INSERT INTO cold_chunks(kind,scope,first_id,last_id,pack,raw_bytes,sha256) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![kind, scope, ids.0, ids.1, pack, raw.len() as i64, sha],
        )?;
        let id = db.last_insert_rowid();
        if kind=="blocks" {index_capture_spans(db,id,&decode(raw)?)?;}
        extra(id)?;
        Ok(id)
    })();
    match result {
        Ok(id) => {
            db.execute_batch("COMMIT")?;
            Ok(id)
        }
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}
/// Reserves `n` consecutive row IDs of an AUTOINCREMENT table. Call inside a write transaction.
pub(crate) fn reserve_ids(db: &Connection, table: &str, n: usize) -> Result<i64> {
    ensure!(matches!(table, "blocks" | "evidence"), "invalid sequence");
    let seq: i64 = db.query_row("SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name=?1),0)", [table], |r| r.get(0))?;
    let max: i64 = db.query_row(&format!("SELECT COALESCE(max(id),0) FROM main.{table}"), [], |r| r.get(0))?;
    let first = seq.max(max) + 1;
    let last = first + n as i64 - 1;
    if db.execute("UPDATE sqlite_sequence SET seq=?2 WHERE name=?1", params![table, last])? == 0 {
        db.execute("INSERT INTO sqlite_sequence(name,seq) VALUES(?1,?2)", params![table, last])?;
    }
    Ok(first)
}
pub(crate) fn silero_start(body: &[u8]) -> Option<(String, u64)> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    timed_silero(&v).map(|(s, t)| (s.to_owned(), t))
}
/// Rows selected for one chunk, the raw HAC3 stream and the oldest wall time in it.
type Built = (Vec<i64>, Vec<u8>, u64);
/// As `Built`, plus the payload bodies that move into the chunk.
type BuiltEvidence = (Vec<i64>, Vec<String>, Vec<u8>, u64);
/// Packs up to `PACK_ROWS` cold timeline rows of one segment: built on `db` (a read snapshot), published by one
/// writer job. Returns rows packed.
pub fn pack_blocks(db: &Connection, queue: &crate::live::Queue, root: &Path, cutoff: u64) -> Result<usize> {
    let segment: Option<i64> = db
        .prepare_cached("SELECT segment FROM blocks INDEXED BY block_age WHERE codec IN ('cold-ref','zero') AND wall_ns<=?1 AND wall_ns+frames*1000000000/rate<=?1 LIMIT 1")?
        .query_row([cutoff as i64], |r| r.get(0))
        .optional()?;
    let Some(segment) = segment else { return Ok(0) };
    db.execute_batch("BEGIN")?;
    let built = (|| -> Result<Option<Built>> {
        let ids: Vec<i64> = db
            .prepare_cached("SELECT id FROM blocks WHERE codec IN ('cold-ref','zero') AND segment=?1 AND wall_ns+frames*1000000000/rate<=?2 ORDER BY id LIMIT ?3")?
            .query_map(params![segment, cutoff as i64, PACK_ROWS], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if ids.is_empty() {
            return Ok(None);
        }
        temp_ids(db, &ids)?;
        let wall: i64 = db.query_row("SELECT min(b.wall_ns) FROM temp.pack_ids p CROSS JOIN blocks b ON b.id=p.id", [], |r| r.get(0))?;
        let mut raw = b"HAC3".to_vec();
        table(db, &mut raw, "blocks", "SELECT b.* FROM temp.pack_ids p CROSS JOIN main.blocks b ON b.id=p.id ORDER BY p.id", &HashMap::new())?;
        Ok(Some((ids, raw, wall.max(0) as u64)))
    })();
    // A read snapshot that only staged temp rows: nothing to keep (a COMMIT would count as an index write).
    db.execute_batch("ROLLBACK")?;
    let Some((ids, raw, wall)) = built? else {
        // Eligible by start but not by end: nothing to pack in this segment yet.
        return Ok(0);
    };
    let (pack, sha, data) = publish_chunk(root, &raw, wall)?;
    let tables = decode(&raw)?;
    let n = ids.len();
    let (r, p, h, raw_len) = (root.to_path_buf(), pack.clone(), sha.clone(), raw.len() as i64);
    let published = queue.background(move |db| {
        store::ensure_blob(&r, &p, &store::chunk_key(&h), &data)?;
        temp_ids(db, &ids)?;
        let still: i64 = db.query_row(
            "SELECT count(*) FROM temp.pack_ids p CROSS JOIN main.blocks b ON b.id=p.id WHERE b.codec IN ('cold-ref','zero')",
            [],
            |r| r.get(0),
        )?;
        ensure!(still as usize == ids.len(), "timeline rows changed while their chunk was built");
        db.execute(
            "INSERT INTO cold_chunks(kind,scope,first_id,last_id,pack,raw_bytes,sha256) VALUES('blocks',?1,?2,?3,?4,?5,?6)",
            params![segment, ids[0], ids[ids.len() - 1], p, raw_len, h],
        )?;
        index_capture_spans(db, db.last_insert_rowid(), &tables)?;
        db.execute("INSERT OR IGNORE INTO packed_audio_refs SELECT DISTINCT object_id FROM main.blocks WHERE id IN (SELECT id FROM temp.pack_ids) AND object_id IS NOT NULL", [])?;
        db.execute("DELETE FROM main.blocks WHERE id IN (SELECT id FROM temp.pack_ids)", [])?;
        Ok(())
    });
    if let Err(e) = published {
        forget_chunk(queue, &pack, &sha);
        return Err(e);
    }
    crate::compact::crash_point("chunk_committed");
    Ok(n)
}
/// A chunk blob whose publication failed is queued for collection (its own writer job).
fn forget_chunk(queue: &crate::live::Queue, pack: &str, sha: &str) {
    let (pack, key) = (pack.to_owned(), store::chunk_key(sha));
    let _ = queue.background(move |db| store::queue_gc(db, &pack, &key));
}

/// Derived geometry only. Caller owns the transaction that publishes the HAC3 container.
fn index_capture_spans(db:&Connection,id:i64,tables:&Tables)->Result<()> {
    let mut spans=std::collections::BTreeMap::<String,(i64,i64)>::new();
    for (name,cols,rows) in tables {
        if name!="blocks" {continue;}
        let col=|name:&str|cols.iter().position(|c|c==name).with_context(||format!("capture column {name}"));
        let source=col("source_id")?;let host=col("host_ns")?;let frames=col("frames")?;let rate=col("rate")?;
        for row in rows {
            let (Value::Text(s),Value::Integer(a),Value::Integer(n),Value::Integer(r))=(&row[source],&row[host],&row[frames],&row[rate])else{anyhow::bail!("capture span geometry");};
            ensure!(*r>0 && *n>=0,"capture span rate/frames");
            let b=a+n*1_000_000_000/r;
            spans.entry(s.clone()).and_modify(|p|{p.0=p.0.min(*a);p.1=p.1.max(b);}).or_insert((*a,b));
        }
    }
    for (source,(a,b)) in spans {
        db.execute("INSERT OR REPLACE INTO main.cold_capture_spans VALUES(?1,?2,?3,?4)",params![id,source,a,b])?;
    }
    db.execute("INSERT OR IGNORE INTO main.cold_capture_indexed VALUES(?1)",[id])?;Ok(())
}
/// Historical geometry migration, at most sixteen containers with admission between each.
pub fn backfill_capture_spans(root:&Path,more:&mut impl FnMut()->bool)->Result<usize> {
    let db=store::open(root)?;
    let ids:Vec<i64>=db.prepare("SELECT c.id FROM main.cold_chunks c WHERE c.kind='blocks' AND NOT EXISTS(SELECT 1 FROM main.cold_capture_indexed i WHERE i.chunk_id=c.id) ORDER BY c.id LIMIT 16")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    let _reader=crate::compact::reader_lock(root)?;let mut n=0;
    for id in ids {
        if !more() {break;}
        let tables=load(&db,root,id)?;
        crate::timeline::transaction(&db,||index_capture_spans(&db,id,&tables))?;n+=1;
    }
    Ok(n)
}

fn timed_silero(v: &serde_json::Value) -> Option<(&str, u64)> {
    if v["model"] != "silero" {
        return None;
    }
    Some((v["input"]["source"].as_str()?, v["input"]["start_ns"].as_u64()?))
}

/// Packs up to `PACK_ROWS` old evidence rows of one session with the payload bodies that only old rows use: built
/// on `db` (a read snapshot), published by one writer job. Returns rows packed.
pub fn pack_evidence(db: &Connection, queue: &crate::live::Queue, root: &Path, cutoff: u64) -> Result<usize> {
    let session: Option<i64> = db
        .prepare_cached("SELECT session FROM evidence WHERE wall_ns<=?1 AND id<>coalesce((SELECT max(id) FROM evidence WHERE kind='send_context'),-1) LIMIT 1")?
        .query_row([cutoff as i64], |r| r.get(0))
        .optional()?;
    let Some(session) = session else { return Ok(0) };
    db.execute_batch("BEGIN")?;
    let built = (|| -> Result<Option<BuiltEvidence>> {
        let mut ids: Vec<i64> = db
            .prepare_cached("SELECT id FROM evidence WHERE session=?1 AND wall_ns<=?2 AND id<>coalesce((SELECT max(id) FROM evidence WHERE kind='send_context'),-1) ORDER BY id LIMIT ?3")?
            .query_map(params![session, cutoff as i64, PACK_ROWS], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        if ids.is_empty() {
            return Ok(None);
        }
        temp_ids(db, &ids)?;
        // The longest run of rows whose distinct live bodies fit the budget (at least one row).
        let sizes: Vec<(String, i64)> = db
            .prepare("SELECT e.payload_id,COALESCE(p.raw_bytes,0) FROM temp.pack_ids i CROSS JOIN main.evidence e ON e.id=i.id LEFT JOIN main.payloads p ON p.id=e.payload_id ORDER BY i.id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut seen = std::collections::HashSet::new();
        let mut total = 0i64;
        let mut keep = 0;
        for (payload, raw) in &sizes {
            let add = if seen.insert(payload.as_str()) { *raw } else { 0 };
            if keep > 0 && total + add > PAYLOAD_BUDGET {
                break;
            }
            total += add;
            keep += 1;
        }
        if keep < ids.len() {
            ids.truncate(keep);
            temp_ids(db, &ids)?;
        }
        db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS pack_payloads(id TEXT PRIMARY KEY); DELETE FROM temp.pack_payloads;")?;
        db.execute("INSERT INTO temp.pack_payloads SELECT DISTINCT p.id FROM temp.pack_ids i CROSS JOIN main.evidence e ON e.id=i.id CROSS JOIN main.payloads p ON p.id=e.payload_id WHERE p.codec IN ('json-zstd-dict-v1','json-zstd-dict-v1-cold') AND NOT EXISTS(SELECT 1 FROM main.evidence h WHERE h.payload_id=p.id AND h.wall_ns>?1)", [cutoff as i64])?;
        let payloads: Vec<(String, i64, Vec<u8>)> = db
            .prepare("SELECT p.id,p.raw_bytes,p.data FROM temp.pack_payloads i CROSS JOIN main.payloads p ON p.id=i.id")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS pack_raw_payloads(id TEXT,raw_bytes INTEGER,codec TEXT,data BLOB); DELETE FROM temp.pack_raw_payloads;")?;
        let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
        let mut moved = vec![];
        for (id, n, data) in payloads {
            let bytes = dec.decompress(&data, n as usize)?;
            ensure!(hash(&bytes) == id, "payload checksum");
            db.execute("INSERT INTO temp.pack_raw_payloads VALUES(?1,?2,'json-raw-chunk-v1',?3)", params![id, n, bytes])?;
            moved.push(id);
        }
        let ordinals: HashMap<String, usize> = db
            .prepare("SELECT id FROM temp.pack_raw_payloads ORDER BY id")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();
        let wall: i64 = db.query_row("SELECT min(e.wall_ns) FROM temp.pack_ids p CROSS JOIN main.evidence e ON e.id=p.id", [], |r| r.get(0))?;
        let mut raw = b"HAC3".to_vec();
        table(db, &mut raw, "payloads", "SELECT id,raw_bytes,codec,data FROM temp.pack_raw_payloads ORDER BY id", &ordinals)?;
        table(db, &mut raw, "evidence", "SELECT e.* FROM temp.pack_ids p CROSS JOIN main.evidence e ON e.id=p.id ORDER BY p.id", &ordinals)?;
        Ok(Some((ids, moved, raw, wall.max(0) as u64)))
    })();
    db.execute_batch("ROLLBACK")?;
    let Some((ids, moved, raw, wall)) = built? else { return Ok(0) };
    let (pack, sha, data) = publish_chunk(root, &raw, wall)?;
    // Timed VAD bodies are located by source/input clock; other bodies keep a placeholder row
    // that names the chunk holding them.
    let mut timed = std::collections::BTreeMap::<String, (u64, u64)>::new();
    let mut silero = std::collections::HashSet::new();
    for (name, cols, rows) in decode(&raw)? {
        if name != "payloads" {
            continue;
        }
        let data_col = cols.iter().position(|c| c == "data").context("payload data column")?;
        for row in rows {
            let (Value::Text(id), Value::Blob(body)) = (&row[0], &row[data_col]) else { continue };
            let value: serde_json::Value = serde_json::from_slice(body)?;
            if let Some((source, start)) = timed_silero(&value) {
                timed
                    .entry(source.to_owned())
                    .and_modify(|r| {
                        r.0 = r.0.min(start);
                        r.1 = r.1.max(start);
                    })
                    .or_insert((start, start));
                silero.insert(id.clone());
            }
        }
    }
    let n = ids.len();
    let (r, p, h, raw_len) = (root.to_path_buf(), pack.clone(), sha.clone(), raw.len() as i64);
    let published = queue.background(move |db| {
        let (root, pack, sha) = (&r, &p, &h);
        store::ensure_blob(root, pack, &store::chunk_key(sha), &data)?;
        temp_ids(db, &ids)?;
        db.execute_batch("CREATE TEMP TABLE IF NOT EXISTS pack_payloads(id TEXT PRIMARY KEY); DELETE FROM temp.pack_payloads;")?;
        let mut keep = db.prepare_cached("INSERT OR IGNORE INTO temp.pack_payloads VALUES(?1)")?;
        for id in &moved {
            keep.execute([id])?;
        }
        let still: i64 = db.query_row("SELECT count(*) FROM temp.pack_ids p CROSS JOIN main.evidence e ON e.id=p.id", [], |r| r.get(0))?;
        ensure!(still as usize == ids.len(), "evidence rows changed while their chunk was built");
        for id in &moved {
            let hot: bool = db
                .prepare_cached("SELECT EXISTS(SELECT 1 FROM main.evidence WHERE payload_id=?1 AND wall_ns>?2)")?
                .query_row(params![id, cutoff as i64], |r| r.get(0))?;
            ensure!(!hot, "a packed payload gained a new hot reference");
        }
        db.execute(
            "INSERT INTO cold_chunks(kind,scope,first_id,last_id,pack,raw_bytes,sha256) VALUES('evidence',?1,?2,?3,?4,?5,?6)",
            params![session, ids[0], ids[ids.len() - 1], pack, raw_len, sha],
        )?;
        let chunk_id = db.last_insert_rowid();
        // A body these rows use but another row still holds live is now referenced from a chunk:
        // pin it (chunk 0) so it is never dropped when its own rows are packed.
        db.execute(
            "UPDATE main.payloads SET chunk=0 WHERE chunk IS NULL AND codec IN ('json-zstd-dict-v1','json-zstd-dict-v1-cold') AND id IN (SELECT e.payload_id FROM temp.pack_ids p CROSS JOIN main.evidence e ON e.id=p.id) AND id NOT IN (SELECT id FROM temp.pack_payloads)",
            [],
        )?;
        db.execute("DELETE FROM main.evidence WHERE id IN (SELECT id FROM temp.pack_ids)", [])?;
        // Moved bodies leave `payloads`. A pointer to the chunk stays in `packed_payloads`, except for a
        // timed VAD body nothing else uses: its source and input clock find the chunk.
        for id in &moved {
            let (raw, pinned, live): (i64, bool, bool) = db
                .prepare_cached("SELECT raw_bytes,chunk IS NOT NULL,EXISTS(SELECT 1 FROM main.evidence WHERE payload_id=?1) FROM main.payloads WHERE id=?1")?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            if !silero.contains(id) || pinned || live {
                db.prepare_cached("INSERT OR IGNORE INTO main.packed_payloads VALUES(?1,?2,?3)")?.execute(params![id, raw, chunk_id])?;
            }
            db.prepare_cached("DELETE FROM main.payloads WHERE id=?1")?.execute([id])?;
        }
        for (source, (a, b)) in &timed {
            db.execute("INSERT OR IGNORE INTO cold_sources VALUES(?1,?2,?3,?4)", params![chunk_id, source, *a as i64, *b as i64])?;
        }
        Ok(())
    });
    if let Err(e) = published {
        forget_chunk(queue, &pack, &sha);
        return Err(e);
    }
    crate::compact::crash_point("chunk_committed");
    Ok(n)
}

/// Timed VAD outputs need no permanent per-frame hash catalogue: their input clock finds candidate
/// chunks; only a repeated old input decodes one. Returns the chunk holding the body.
///
/// The input clock is the host clock, which starts again at every boot, so chunks of an earlier boot can cover the
/// current time for a source of the same name: then every new frame (about 31 a second per source) found the same
/// chunks, and each was read, decompressed, hashed and decoded again inside the evidence writer's transaction, with
/// the archive's write lock held. A chunk's payload IDs are kept per content hash (`chunk_payloads`), so each chunk is
/// decoded once; `prefetch` does it before the lock.
pub fn payload_is_packed(db: &Connection, root: &Path, id: &str, payload: &serde_json::Value) -> Result<Option<i64>> {
    for (chunk, sha) in candidates(db, payload)? {
        if chunk_payloads(db, root, chunk, &sha)?.contains(id) {
            return Ok(Some(chunk));
        }
    }
    Ok(None)
}
/// Reads (outside any write lock) the chunks `payload_is_packed` will ask about for `payload`, so its lookup inside the
/// lock is an index query and a hash lookup.
pub fn prefetch(db: &Connection, root: &Path, payload: &serde_json::Value) -> Result<()> {
    for (chunk, sha) in candidates(db, payload)? {
        chunk_payloads(db, root, chunk, &sha)?;
    }
    Ok(())
}
/// The cold chunks (ID, content hash) whose source span covers a timed VAD payload's input time.
fn candidates(db: &Connection, payload: &serde_json::Value) -> Result<Vec<(i64, String)>> {
    let Some((source, start)) = timed_silero(payload) else {
        return Ok(vec![]);
    };
    let chunks: Vec<(i64, String)> = db
        .prepare_cached("SELECT s.chunk_id,c.sha256 FROM cold_sources s INDEXED BY cold_source_end JOIN main.cold_chunks c ON c.id=s.chunk_id WHERE s.source=?1 AND s.start_ns<=?2 AND s.end_ns>=?2")?
        .query_map(params![source, start as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(chunks)
}
/// Chunks whose payload IDs are kept, most recently used first: about 1 MB each at 8,192 rows. A frame's time falls in
/// about one chunk per earlier boot (the host clock is monotonic within a boot), far fewer than this.
const KEPT_CHUNKS: usize = 32;
fn kept() -> &'static std::sync::Mutex<std::collections::VecDeque<(String, std::sync::Arc<std::collections::HashSet<String>>)>> {
    static KEPT: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<(String, std::sync::Arc<std::collections::HashSet<String>>)>>> = std::sync::OnceLock::new();
    KEPT.get_or_init(Default::default)
}
static DECODED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Chunks `payload_is_packed` has decoded in this process (tests).
pub fn payload_chunk_decodes() -> usize {
    DECODED.load(std::sync::atomic::Ordering::Relaxed)
}
/// The payload IDs in chunk `chunk`, whose content hash is `sha` (immutable: a chunk is never rewritten in place).
fn chunk_payloads(db: &Connection, root: &Path, chunk: i64, sha: &str) -> Result<std::sync::Arc<std::collections::HashSet<String>>> {
    {
        let mut kept = kept().lock().unwrap();
        if let Some(i) = kept.iter().position(|(s, _)| s == sha) {
            let hit = kept.remove(i).unwrap();
            let ids = hit.1.clone();
            kept.push_front(hit);
            return Ok(ids);
        }
    }
    let tables = load(db, root, chunk)?;
    DECODED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ids: std::collections::HashSet<String> = tables
        .iter()
        .filter(|(name, _, _)| name == "payloads")
        .flat_map(|(_, _, rows)| rows.iter().filter_map(|row| match &row[0] {
            Value::Text(id) => Some(id.clone()),
            _ => None,
        }))
        .collect();
    let ids = std::sync::Arc::new(ids);
    let mut kept = kept().lock().unwrap();
    if !kept.iter().any(|(s, _)| s == sha) {
        kept.push_front((sha.to_owned(), ids.clone()));
        kept.truncate(KEPT_CHUNKS);
    }
    Ok(ids)
}
fn payload_in(tables: &Tables, id: &str) -> Option<(i64, Vec<u8>)> {
    for (name, cols, rows) in tables {
        if name != "payloads" {
            continue;
        }
        let data = cols.iter().position(|c| c == "data")?;
        let raw = cols.iter().position(|c| c == "raw_bytes")?;
        for row in rows {
            if matches!(&row[0], Value::Text(x) if x == id) {
                if let (Value::Integer(n), Value::Blob(b)) = (&row[raw], &row[data]) {
                    return Some((*n, b.clone()));
                }
            }
        }
    }
    None
}
/// The body of a payload that lives in a chunk (`packed_payloads` names it).
pub fn packed_body(db: &Connection, root: &Path, id: &str) -> Result<Vec<u8>> {
    let chunk: Option<i64> = db
        .prepare_cached("SELECT chunk FROM main.packed_payloads WHERE id=?1")?
        .query_row([id], |r| r.get(0))
        .optional()?;
    let chunk = chunk.with_context(|| format!("payload {id} has no chunk"))?;
    let (_, body) = payload_in(&load(db, root, chunk)?, id).with_context(|| format!("payload {id} missing from chunk {chunk}"))?;
    ensure!(hash(&body) == id, "packed payload checksum");
    Ok(body)
}
/// One timeline row by ID, live or packed, as `(column, value)` pairs.
pub fn find_block(db: &Connection, root: &Path, id: i64) -> Result<Option<Vec<(String, Value)>>> {
    // Chunks of different segments may interleave ID ranges: check every candidate.
    let chunks: Vec<i64> = db
        .prepare_cached("SELECT id FROM main.cold_chunks WHERE kind='blocks' AND first_id<=?1 AND last_id>=?1 ORDER BY first_id DESC")?
        .query_map([id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    for chunk in chunks {
        for (name, cols, rows) in load(db, root, chunk)? {
            if name != "blocks" {
                continue;
            }
            let id_col = cols.iter().position(|c| c == "id").context("block id column")?;
            for row in rows {
                if matches!(row[id_col], Value::Integer(x) if x == id) {
                    return Ok(Some(cols.iter().cloned().zip(row).collect()));
                }
            }
        }
    }
    Ok(None)
}
