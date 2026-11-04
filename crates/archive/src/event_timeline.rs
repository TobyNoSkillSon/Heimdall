//! Agent-facing decoded event timeline. Every persistent SQLite handle is READ_ONLY.
//! TEMP tables stay in memory; we never initialize schema or call store::open/get.
use crate::{
    chunks,
    records::{DICTIONARY, payload_body},
    store,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
};
#[derive(Default)]
pub struct Options {
    pub since: u64,
    pub until: u64,
    pub kinds: BTreeSet<String>,
    pub problems: bool,
}
fn selected(opt: &Options, kind: &str) -> bool {
    if opt.kinds.is_empty() {
        kind != "model_output" || opt.problems
    } else {
        opt.kinds.contains(kind)
    }
}
fn retain_window(tables: &mut chunks::Tables, opt: &Options) -> Result<()> {
    let mut wanted = BTreeSet::new();
    for (name, cols, rows) in tables.iter_mut() {
        if name != "evidence" {
            continue;
        }
        let wall = cols
            .iter()
            .position(|c| c == "wall_ns")
            .context("event wall time")?;
        let kind = cols
            .iter()
            .position(|c| c == "kind")
            .context("event kind")?;
        let payload = cols
            .iter()
            .position(|c| c == "payload_id")
            .context("event payload")?;
        rows.retain(|r|matches!((&r[wall],&r[kind]),(rusqlite::types::Value::Integer(w),rusqlite::types::Value::Text(k)) if *w>=opt.since as i64 && *w<opt.until as i64 && selected(opt,k)));
        for row in rows {
            if let rusqlite::types::Value::Text(id) = &row[payload] {
                wanted.insert(id.clone());
            }
        }
    }
    for (name, cols, rows) in tables {
        if name == "payloads" {
            let id = cols.iter().position(|c| c == "id").context("payload id")?;
            rows.retain(
                |r| matches!(&r[id],rusqlite::types::Value::Text(id) if wanted.contains(id)),
            );
        }
    }
    Ok(())
}
fn readonly(path: &Path) -> Result<Connection> {
    // SQLite can create WAL sidecars even for a read-only main database. Refuse that
    // case instead of quietly changing the live archive or ignoring committed WAL.
    let header = std::fs::File::open(path)?;
    use std::io::Read;
    let mut h = [0; 20];
    (&header).read_exact(&mut h)?;
    if h[18] == 2 {
        for suffix in ["-wal", "-shm"] {
            ensure!(
                PathBuf::from(format!("{}{suffix}", path.display())).exists(),
                "WAL sidecar {suffix} absent for {}; use a consistent isolated snapshot (no sidecars will be created)",
                path.display()
            );
        }
    }
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(std::time::Duration::from_millis(250))?;
    db.execute_batch("PRAGMA temp_store=MEMORY; BEGIN")?;
    Ok(db)
}
fn load_chunk(db: &Connection, root: &Path, id: i64) -> Result<chunks::Tables> {
    let (pack, raw, sha): (String, i64, String) = db.query_row(
        "SELECT pack,raw_bytes,sha256 FROM cold_chunks WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    ensure!((0..=128 * 1024 * 1024).contains(&raw), "chunk decode bound");
    let pack = readonly(&store::pack_path(root, &pack)?)?;
    let data: Vec<u8> = pack.query_row(
        "SELECT data FROM blobs WHERE key=?1",
        [store::chunk_key(&sha)],
        |r| r.get(0),
    )?;
    let bytes =
        zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?.decompress(&data, raw as usize)?;
    ensure!(crate::hash(&bytes) == sha, "chunk checksum mismatch");
    chunks::decode(&bytes)
}
fn body(
    db: &Connection,
    root: &Path,
    id: &str,
    dec: &mut zstd::bulk::Decompressor<'_>,
) -> Result<Value> {
    let row: Option<(usize, String, Vec<u8>)> = db
        .query_row(
            "SELECT raw_bytes,codec,data FROM temp.event_payloads WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, i64>(0)?.max(0) as usize, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((raw, codec, data)) = row {
        if codec != "json-packed-v1" {
            return Ok(serde_json::from_slice(&payload_body(
                db, root, id, raw, &codec, data, dec,
            )?)?);
        }
    }
    let chunk: i64 = db.query_row("SELECT chunk FROM packed_payloads WHERE id=?1", [id], |r| {
        r.get(0)
    })?;
    for (name, cols, rows) in load_chunk(db, root, chunk)? {
        if name != "payloads" {
            continue;
        }
        let i = cols.iter().position(|c| c == "id").context("payload id")?;
        let d = cols
            .iter()
            .position(|c| c == "data")
            .context("payload body")?;
        for row in rows {
            if row[i] == rusqlite::types::Value::Text(id.into()) {
                if let rusqlite::types::Value::Blob(bytes) = &row[d] {
                    ensure!(crate::hash(bytes) == id, "payload checksum mismatch");
                    return Ok(serde_json::from_slice(bytes)?);
                }
            }
        }
    }
    anyhow::bail!("missing payload {id}")
}
pub fn print(root: &Path, opt: &Options, out: &mut impl Write) -> Result<usize> {
    ensure!(opt.until >= opt.since, "until must follow since");
    // Existing lock only: never create a gate/lock file in the user's archive. A
    // shared lock prevents compaction's blob collection during this read snapshot.
    let lock = std::fs::File::open(root.join(".compact.lock")).ok();
    if let Some(file) = &lock {
        use std::os::fd::AsRawFd;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0,
            "archive compactor busy; retry timeline"
        );
    }
    let db = readonly(&root.join(store::INDEX))?;
    db.execute_batch("CREATE TEMP TABLE event_evidence AS SELECT * FROM main.evidence WHERE 0; CREATE UNIQUE INDEX temp.event_id ON event_evidence(id); CREATE TEMP TABLE event_payloads AS SELECT id,raw_bytes,codec,data FROM main.payloads WHERE 0; CREATE UNIQUE INDEX temp.event_payload_id ON event_payloads(id);")?;
    db.execute("INSERT INTO temp.event_evidence SELECT * FROM main.evidence WHERE wall_ns>=?1 AND wall_ns<?2 AND (?3 OR kind!='model_output')",params![opt.since as i64,opt.until as i64,opt.problems || opt.kinds.contains("model_output")])?;
    let ids:Vec<i64>=db.prepare("SELECT c.id FROM cold_chunks c JOIN sessions s ON s.id=c.scope WHERE c.kind='evidence' AND s.started_wall_ns<?2 AND (s.complete_wall_ns IS NULL OR s.complete_wall_ns>=?1) ORDER BY c.id")?.query_map(params![opt.since as i64,opt.until as i64],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
    ensure!(
        ids.is_empty() || lock.is_some(),
        "historical packs require the existing archive .compact.lock; no lock file was created"
    );
    for id in ids {
        let mut tables = load_chunk(&db, root, id)?;
        retain_window(&mut tables, opt)?;
        chunks::insert_rows(&db, &tables, "evidence", "temp.event_evidence")?;
        chunks::insert_rows(&db, &tables, "payloads", "temp.event_payloads")?;
        db.execute(
            "DELETE FROM temp.event_evidence WHERE wall_ns<?1 OR wall_ns>=?2",
            params![opt.since as i64, opt.until as i64],
        )?;
    }
    db.execute_batch("INSERT OR IGNORE INTO temp.event_payloads SELECT id,raw_bytes,codec,data FROM main.payloads WHERE id IN (SELECT payload_id FROM temp.event_evidence)")?;
    // The final transcript projection is independent of model evidence. Include its
    // decoded decision and actual utterance time in the same chronological stream.
    db.execute("INSERT INTO temp.event_evidence SELECT -rowid,session,'utterance',source_id,json_array(id),start_ns,wall_ms*1000000,'utterance:'||session||':'||id FROM main.utterances WHERE wall_ms*1000000>=?1 AND wall_ms*1000000<?2",params![opt.since as i64,opt.until as i64])?;
    db.execute("INSERT OR IGNORE INTO temp.event_payloads SELECT 'utterance:'||session||':'||id,0,'json-raw-chunk-v1',CAST(json_object('utterance_id',id,'text',text,'speaker',speaker,'source',source,'start_ns',start_ns,'end_ns',end_ns,'words',json(words),'decision',json(decision)) AS BLOB) FROM main.utterances WHERE wall_ms*1000000>=?1 AND wall_ms*1000000<?2",params![opt.since as i64,opt.until as i64])?;
    let mut stmt=db.prepare("SELECT id,session,kind,source,utterance_ids,host_ns,wall_ns,payload_id FROM temp.event_evidence ORDER BY wall_ns,id")?;
    let mut rows = stmt.query([])?;
    let mut dec = zstd::bulk::Decompressor::with_dictionary(DICTIONARY)?;
    let mut count = 0;
    while let Some(r) = rows.next()? {
        let kind: String = r.get(2)?;
        if !selected(opt, &kind) {
            continue;
        }
        let payload_id: String = r.get(7)?;
        let payload = match body(&db, root, &payload_id, &mut dec) {
            Ok(p) => p,
            Err(e) => {
                writeln!(
                    out,
                    "{}",
                    json!({"time":local_time(r.get::<_,i64>(6)?.max(0) as u64),"wall_ns":r.get::<_,i64>(6)?,"id":r.get::<_,i64>(0)?,"kind":kind,"payload_id":payload_id,"decode_error":format!("{e:#}")})
                )?;
                count += 1;
                continue;
            }
        };
        if opt.problems && !crate::diagnostics::problem(&kind, &payload) {
            continue;
        }
        let wall = r.get::<_, i64>(6)?.max(0) as u64;
        writeln!(
            out,
            "{}",
            json!({"time":local_time(wall),"wall_ns":wall,"id":r.get::<_,i64>(0)?,"session":r.get::<_,i64>(1)?,"kind":kind,"source":r.get::<_,String>(3)?,"utterance_ids":serde_json::from_str::<Value>(&r.get::<_,String>(4)?)?,"host_ns":r.get::<_,i64>(5)?,"payload":payload})
        )?;
        count += 1;
    }
    Ok(count)
}
fn local_time(ns: u64) -> String {
    unsafe {
        let t = (ns / 1_000_000_000) as libc::time_t;
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec,
            ns / 1_000_000 % 1000
        )
    }
}
fn time_arg(s: &str) -> Result<u64> {
    let now = (crate::wall_ns() / 1_000_000_000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&now, &mut tm);
    }
    let (date, time) = s
        .split_once('T')
        .map(|(d, t)| (Some(d), t))
        .unwrap_or((None, s));
    if let Some(date) = date {
        let d: Vec<i32> = date
            .split('-')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()?;
        ensure!(d.len() == 3, "date must be YYYY-MM-DD");
        tm.tm_year = d[0] - 1900;
        tm.tm_mon = d[1] - 1;
        tm.tm_mday = d[2];
    }
    let t: Vec<i32> = time
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    ensure!(
        (2..=3).contains(&t.len())
            && (0..24).contains(&t[0])
            && (0..60).contains(&t[1])
            && t.get(2).is_none_or(|s| (0..60).contains(s)),
        "time must be HH:MM[:SS]"
    );
    tm.tm_hour = t[0];
    tm.tm_min = t[1];
    tm.tm_sec = *t.get(2).unwrap_or(&0);
    tm.tm_isdst = -1;
    let expected = (
        tm.tm_year, tm.tm_mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec,
    );
    let result = unsafe { libc::mktime(&mut tm) };
    ensure!(
        expected
            == (
                tm.tm_year, tm.tm_mon, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec
            ),
        "invalid or nonexistent local date/time"
    );
    ensure!(result >= 0, "invalid local date");
    Ok(result as u64 * 1_000_000_000)
}
pub fn cli(args: &[String]) -> Result<()> {
    let mut opt = Options {
        until: i64::MAX as u64,
        ..Default::default()
    };
    let mut root = None;
    let mut args = args.iter();
    let mut since = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--since" => {
                opt.since = time_arg(args.next().context("--since HH:MM")?)?;
                since = true
            }
            "--until" => opt.until = time_arg(args.next().context("--until HH:MM")?)?,
            "--kinds" => {
                opt.kinds = args
                    .next()
                    .context("--kinds comma,separated")?
                    .split(',')
                    .map(str::to_owned)
                    .collect()
            }
            "--problems" => opt.problems = true,
            "--archive" => root = Some(PathBuf::from(args.next().context("--archive ROOT")?)),
            _ => anyhow::bail!("unknown timeline argument {arg}"),
        }
    }
    ensure!(
        since,
        "timeline --since HH:MM [--until HH:MM] [--kinds kind,kind] [--problems] [--archive ROOT]"
    );
    let root = root.unwrap_or_else(|| heimdall_home::data_dir().join("archive"));
    print(&root, &opt, &mut std::io::stdout().lock())?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn send_decisions_are_visible_and_slow_ones_are_problems() {
        let t=tempfile::tempdir().unwrap();let j=crate::Journal::open(&t.path().join("session-send-timeline")).unwrap();
        for (at,ms) in [(1,30),(2,501)] {j.event("send_decision","mic",&["turn".into()],at,json!({"prompt":{"state":"User: hello"},"latency_ms":ms,"input_tokens":100,"trigger":"name","outcome":"send"})).unwrap();}
        let mut out=vec![];let mut opt=Options {since:0,until:i64::MAX as u64,..Default::default()};
        assert_eq!(print(t.path(),&opt,&mut out).unwrap(),2);
        let rows:Vec<Value>=String::from_utf8(out).unwrap().lines().map(|l|serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows[0]["payload"]["prompt"]["state"],"User: hello");
        let mut out=vec![];opt.problems=true;assert_eq!(print(t.path(),&opt,&mut out).unwrap(),1);
    }
    #[test]
    fn decoded_live_wal_order_filters_and_no_new_sidecars() {
        let t = tempfile::tempdir().unwrap();
        let j = crate::Journal::open(&t.path().join("session-timeline")).unwrap();
        j.event(
            "voice_reply",
            "mic",
            &["turn".into()],
            1,
            json!({"error":"reply question requires audio"}),
        )
        .unwrap();
        j.event("health", "heimdall", &[], 2, json!({"state":"recovered"}))
            .unwrap();
        let before = std::fs::read(t.path().join(store::INDEX)).unwrap();
        let files = || {
            let mut v: Vec<_> = std::fs::read_dir(t.path())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        let names = files();
        let mut out = vec![];
        let all = Options {
            since: 0,
            until: i64::MAX as u64,
            ..Default::default()
        };
        assert_eq!(print(t.path(), &all, &mut out).unwrap(), 2);
        let lines: Vec<Value> = String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(
            lines[0]["payload"]["error"],
            "reply question requires audio"
        );
        assert_eq!(lines[0]["utterance_ids"][0], "turn");
        let mut out = vec![];
        assert_eq!(
            print(
                t.path(),
                &Options {
                    problems: true,
                    ..all
                },
                &mut out
            )
            .unwrap(),
            1
        );
        assert_eq!(before, std::fs::read(t.path().join(store::INDEX)).unwrap());
        assert_eq!(names, files());
    }
    #[test]
    fn cold_evidence_and_reused_payload_are_decoded_without_writer_connections() {
        let t = tempfile::tempdir().unwrap();
        let j = crate::Journal::open(&t.path().join("session-cold-timeline")).unwrap();
        let record = crate::Record {
            kind: "voice_reply".into(),
            source: "mic".into(),
            utterance_ids: vec![],
            host_ns: 1,
            wall_ns: 1,
            payload: json!({"error":"reply question requires audio"}),
        };
        j.append(&record).unwrap();
        crate::compact::compact_metadata(t.path(), crate::DAY_NS + 100, false).unwrap();
        j.append(&record).unwrap();
        let mut out = vec![];
        assert_eq!(
            print(
                t.path(),
                &Options {
                    since: 0,
                    until: i64::MAX as u64,
                    ..Default::default()
                },
                &mut out
            )
            .unwrap(),
            2
        );
        for line in String::from_utf8(out).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["payload"]["error"], record.payload["error"]);
            assert!(v["decode_error"].is_null());
        }
    }
    #[test]
    fn final_utterance_decision_and_kind_window_are_printed_in_time_order() {
        let t = tempfile::tempdir().unwrap();
        let j = crate::Journal::open(&t.path().join("session-final-timeline")).unwrap();
        let db = store::open(t.path()).unwrap();
        db.execute("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(?1,'turn',1000,'mic','microphone','User',1,2,'take a screenshot','[]','[]','{\"would_wake\":true}')",[j.session_id()]).unwrap();
        let mut out = vec![];
        assert_eq!(
            print(
                t.path(),
                &Options {
                    since: 999_000_000,
                    until: 1_001_000_000,
                    kinds: ["utterance".into()].into(),
                    problems: false
                },
                &mut out
            )
            .unwrap(),
            1
        );
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["payload"]["text"], "take a screenshot");
        assert_eq!(v["payload"]["decision"]["would_wake"], true);
        assert_eq!(v["wall_ns"], 1_000_000_000);
    }
    #[test]
    fn a_closed_wal_database_never_acquires_new_sidecars() {
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("closed.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE sentinel(x)")
            .unwrap();
        drop(db);
        assert!(!PathBuf::from(format!("{}-shm", path.display())).exists());
        assert!(readonly(&path).is_err());
        assert!(!PathBuf::from(format!("{}-shm", path.display())).exists());
    }
    #[test]
    fn time_arguments_reject_invalid_clock() {
        assert!(time_arg("24:00").is_err());
        assert!(time_arg("10:60").is_err());
        assert!(time_arg("10:30").is_ok());
    }
}
