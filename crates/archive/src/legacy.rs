//! Read-only access to the per-launch folder layout that preceded the store, for migration only.
//! A legacy container is a SQLite file (WAL included) or a sealed `.sqlite.zst` stream with the frozen
//! dictionary; packed rows live in HAC1/HAC2 chunks inside its own `cold_chunks` table.
use crate::{hash, records::DICTIONARY};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, types::Value};
use std::{fs::File, path::Path};

/// An opened legacy container with its packed rows expanded into TEMP views that shadow the
/// original table names (`blocks`, `evidence`, `payloads`).
pub struct Container {
    pub db: Connection,
    _temp: Option<tempfile::TempDir>,
}
impl Container {
    pub fn open(path: &Path) -> Result<Self> {
        let compressed = path.to_string_lossy().ends_with(".sqlite.zst");
        let (actual, temp) = if compressed {
            let temp = tempfile::Builder::new().prefix("heimdall-legacy-").tempdir()?;
            let decoded = temp.path().join("legacy.sqlite");
            let mut decoder = zstd::stream::read::Decoder::with_dictionary(
                std::io::BufReader::new(File::open(path)?),
                DICTIONARY,
            )?;
            let mut file = File::create(&decoded)?;
            std::io::copy(&mut decoder, &mut file)?;
            drop(file);
            (decoded, Some(temp))
        } else {
            (path.to_path_buf(), None)
        };
        // Read-only keeps the legacy file untouched; a WAL without its -shm needs a normal open.
        let db = match Connection::open_with_flags(&actual, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(db) if db.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get::<_, i64>(0)).is_ok() => db,
            _ => Connection::open(&actual)?,
        };
        db.busy_timeout(std::time::Duration::from_secs(10))?;
        db.execute_batch("PRAGMA temp_store=FILE")?;
        ensure!(
            db.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? == "ok",
            "legacy SQLite {} failed its integrity check",
            path.display()
        );
        expand(&db)?;
        Ok(Self { db, _temp: temp })
    }
    pub fn has_table(&self, name: &str) -> Result<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1 UNION ALL SELECT 1 FROM sqlite_temp_master WHERE name=?1)",
            [name],
            |r| r.get(0),
        )?)
    }
    pub fn columns(&self, table: &str) -> Result<Vec<String>> {
        Ok(self
            .db
            .prepare(&format!("SELECT * FROM {table} LIMIT 0"))?
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect())
    }
}

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
    fn row(&mut self, cols: usize, ids: &[String]) -> Result<Vec<Value>> {
        let mut values = vec![];
        let mut derived = false;
        for col in 0..cols {
            let tag = self.take(1)?[0];
            values.push(match tag {
                0 => Value::Null,
                1 => Value::Integer(i64::from_le_bytes(self.take(8)?.try_into()?)),
                2 => Value::Real(f64::from_le_bytes(self.take(8)?.try_into()?)),
                3 => Value::Text(std::str::from_utf8(self.blob()?)?.to_owned()),
                4 => Value::Blob(self.blob()?.to_vec()),
                5 => {
                    ensure!(col == 0, "derived identity column");
                    derived = true;
                    Value::Null
                }
                6 => {
                    let n = self.u32()?;
                    Value::Text(ids.get(n).context("payload ordinal")?.clone())
                }
                _ => anyhow::bail!("chunk value"),
            });
        }
        if derived {
            let Some(Value::Blob(bytes)) = values.get(3) else {
                anyhow::bail!("derived payload body")
            };
            values[0] = Value::Text(hash(bytes));
        }
        Ok(values)
    }
}
/// TEMP views union packed rows with the live main tables, as the legacy readers did.
fn expand(db: &Connection) -> Result<()> {
    let has: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='cold_chunks')",
        [],
        |r| r.get(0),
    )?;
    if !has {
        return Ok(());
    }
    let mut tables = std::collections::BTreeSet::new();
    let mut stmt = db.prepare("SELECT raw_bytes,sha256,data FROM main.cold_chunks ORDER BY id")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let n = row.get::<_, i64>(0)?;
        ensure!((0..=128 * 1024 * 1024).contains(&n), "chunk bound");
        let digest: String = row.get(1)?;
        let data: Vec<u8> = row.get(2)?;
        let raw = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?.decompress(&data, n as usize)?;
        ensure!(hash(&raw) == digest, "chunk checksum");
        let mut r = Reader { bytes: &raw, pos: 0 };
        let version = r.take(4)?;
        ensure!(version == b"HAC1" || version == b"HAC2", "chunk version");
        let mut ids = vec![];
        while r.pos < raw.len() {
            let name = std::str::from_utf8(r.blob()?)?;
            ensure!(matches!(name, "blocks" | "evidence" | "payloads"), "chunk table");
            if tables.insert(name.to_string()) {
                db.execute_batch(&format!(
                    "CREATE TEMP TABLE cold_{name} AS SELECT * FROM main.{name} WHERE 0"
                ))?;
            }
            let cols = r.u32()?;
            let count = r.u32()?;
            ensure!(cols <= 16 && count <= 8192, "chunk geometry");
            let sql = format!("INSERT INTO temp.cold_{name} VALUES({})", vec!["?"; cols].join(","));
            let mut insert = db.prepare(&sql)?;
            for _ in 0..count {
                let values = r.row(cols, &ids)?;
                if name == "payloads" {
                    let Value::Text(id) = &values[0] else {
                        anyhow::bail!("payload identity")
                    };
                    ids.push(id.clone());
                }
                insert.execute(rusqlite::params_from_iter(values))?;
            }
        }
    }
    for name in tables {
        if name == "payloads" {
            // Migration looks bodies up by ID.
            db.execute_batch("CREATE INDEX IF NOT EXISTS temp.cold_payloads_id ON cold_payloads(id)")?;
        }
        let filter = if name == "payloads" { " WHERE codec<>'json-packed-v1'" } else { "" };
        db.execute_batch(&format!(
            "CREATE TEMP VIEW {name} AS SELECT * FROM main.{name}{filter} UNION ALL SELECT * FROM temp.cold_{name}"
        ))?;
    }
    Ok(())
}
