//! Indexed, bounded-memory joins. Raw records remain once in evidence.jsonl.
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{BufRead, BufReader, BufWriter, Write},
    path::Path,
};

fn ids(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key == "utterance_id" || key == "utterance_ids" {
                    if let Some(id) = value.as_str() {
                        out.insert(id.to_owned());
                    }
                    if let Some(list) = value.as_array() {
                        for id in list.iter().filter_map(Value::as_str) {
                            out.insert(id.to_owned());
                        }
                    }
                }
                // Bug context and agent observations use the same utterance id field.
                if key == "id" && object.contains_key("text") {
                    if let Some(id) = value.as_str() {
                        out.insert(id.to_owned());
                    }
                }
                ids(value, out);
            }
        }
        Value::Array(list) => {
            for value in list {
                ids(value, out);
            }
        }
        _ => {}
    }
}
fn span(record: &Value) -> (u64, u64) {
    let fallback = record["host_ns"].as_u64().unwrap_or(0);
    let payload = &record["payload"];
    for input in [payload.get("audio"), payload.get("input"), Some(payload)]
        .into_iter()
        .flatten()
    {
        if let (Some(a), Some(b)) = (
            input["start_seconds"].as_f64(),
            input["end_seconds"].as_f64(),
        ) {
            if a.is_finite() && b.is_finite() && a >= 0. && b >= a {
                return ((a * 1e9).round() as u64, (b * 1e9).round() as u64);
            }
        }
        let a = input["start_ns"].as_u64();
        let b = input["end_ns"].as_u64();
        if let (Some(a), Some(b)) = (a, b) {
            if b >= a {
                return (a, b);
            }
        }
    }
    (fallback, fallback)
}

pub(super) fn join(output: &Path) -> Result<()> {
    let temp = tempfile::tempdir_in(output)?;
    let db = Connection::open(temp.path().join("join.sqlite"))?;
    db.execute_batch(
        "PRAGMA temp_store=FILE; PRAGMA cache_size=-4096;
      CREATE TABLE events(session TEXT,id TEXT,kind TEXT,source TEXT,a INTEGER,b INTEGER);
      CREATE INDEX event_span ON events(session,source,a,b);
      CREATE TABLE links(session TEXT,utterance TEXT,event TEXT);
      CREATE INDEX link_utterance ON links(session,utterance);",
    )?;
    let mut events = BufWriter::new(File::create(temp.path().join("evidence.jsonl"))?);
    let mut n = 0_u64;
    for line in BufReader::new(File::open(output.join("evidence.jsonl"))?).lines() {
        let mut value: Value = serde_json::from_str(&line?)?;
        n += 1;
        let session = value["session"].as_str().context("evidence session")?;
        let id = value
            .get("event_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{session}:event:{n}"));
        let record = value.get("record").unwrap_or(&value);
        let kind = record["kind"].as_str().unwrap_or("unknown");
        let source = record["source"].as_str().unwrap_or("");
        let (a, b) = span(record);
        db.execute(
            "INSERT INTO events VALUES(?1,?2,?3,?4,?5,?6)",
            params![session, id, kind, source, a as i64, b as i64],
        )?;
        let mut linked = BTreeSet::new();
        ids(record, &mut linked);
        for utterance in linked {
            db.execute(
                "INSERT INTO links VALUES(?1,?2,?3)",
                params![session, utterance, id],
            )?;
        }
        value["event_id"] = json!(id);
        writeln!(events, "{value}")?;
    }
    events.flush()?;
    let mut manifest = BufWriter::new(File::create(temp.path().join("manifest.jsonl"))?);
    for line in BufReader::new(File::open(output.join("manifest.jsonl"))?).lines() {
        let mut row: Value = serde_json::from_str(&line?)?;
        let mut query = db.prepare(
            "SELECT DISTINCT e.id,e.kind FROM events e WHERE e.session=?1 AND
          (e.id IN (SELECT event FROM links WHERE session=?1 AND utterance=?2)
          OR (NOT EXISTS(SELECT 1 FROM links l WHERE l.session=e.session AND l.event=e.id) AND e.source=?3 AND e.a<=?5 AND e.b>=?4)) ORDER BY e.id",
        )?;
        let refs = query
            .query_map(
                params![
                    row["session"].as_str(),
                    row["utterance_id"].as_str(),
                    row["source_id"].as_str(),
                    row["start_ns"].as_u64().unwrap_or(0) as i64,
                    row["end_ns"].as_u64().unwrap_or(0) as i64
                ],
                |r| Ok(json!({"event_id":r.get::<_,String>(0)?,"kind":r.get::<_,String>(1)?})),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        row["schema"] = json!(2);
        row["evidence_refs"] = json!(refs);
        writeln!(manifest, "{row}")?;
    }
    manifest.flush()?;
    drop(manifest);
    drop(events);
    std::fs::rename(
        temp.path().join("manifest.jsonl"),
        output.join("manifest.jsonl"),
    )?;
    std::fs::rename(
        temp.path().join("evidence.jsonl"),
        output.join("evidence.jsonl"),
    )?;
    Ok(())
}

/// Additional outputs have content-addressed audio and session-qualified evidence.
pub(super) fn additional(root: &Path, output: &Path, events: &mut File) -> Result<()> {
    let mut files = vec![root.join("evidence.jsonl")];
    if root.join("phases").is_dir() {
        let mut phases = std::fs::read_dir(root.join("phases"))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        phases.sort();
        files.extend(
            phases
                .into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "json")),
        );
    }
    let mut seen = std::collections::HashSet::new();
    for file in files {
        for line in BufReader::new(File::open(file)?).lines() {
            let mut row: Value = serde_json::from_str(&line?)?;
            let id = row["event_id"]
                .as_str()
                .context("night event id")?
                .to_owned();
            if !seen.insert(id) {
                continue;
            }
            for track in row["record"]["payload"]["tracks"]
                .as_array_mut()
                .into_iter()
                .flatten()
            {
                let relative = track["audio"].as_str().context("night track audio")?;
                let name = Path::new(relative)
                    .file_name()
                    .context("night audio name")?;
                anyhow::ensure!(
                    Path::new(relative)
                        .components()
                        .all(|c| matches!(c, std::path::Component::Normal(_))),
                    "unsafe night audio path"
                );
                let bytes = std::fs::read(root.join(relative))?;
                anyhow::ensure!(
                    track["audio_sha256"].as_str() == Some(&crate::hash(&bytes)),
                    "night audio hash mismatch"
                );
                let dest = Path::new("audio").join(name);
                if output.join(&dest).exists() {
                    anyhow::ensure!(
                        std::fs::read(output.join(&dest))? == bytes,
                        "audio collision"
                    );
                } else {
                    crate::atomic_write(&output.join(&dest), &bytes)?;
                }
                track["audio"] = json!(dest);
            }
            writeln!(events, "{row}")?;
        }
    }
    Ok(())
}
