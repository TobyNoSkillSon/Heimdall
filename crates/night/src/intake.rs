//! Metadata first: no archive snapshot, media copy, or whole-archive WAV expansion.
use crate::storage;
use anyhow::{ensure, Context, Result};
use heimdall_archive::store;
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
fn line(file: &mut std::fs::File, root: &Path, value: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&value)?;
    bytes.push(b'\n');
    storage::require(root, bytes.len() as u64)?;
    file.write_all(&bytes)?;
    Ok(())
}
/// Pin blob lifetimes while the selected jobs use native references (the store's shared
/// lifetime lock). Live appenders do not acquire it.
pub fn pin(archive: &Path) -> Result<Vec<heimdall_archive::compact::ReaderLock>> {
    if !store::exists(archive) {
        return Ok(vec![]);
    }
    Ok(vec![heimdall_archive::compact::reader_lock(archive)?])
}
/// Schema-3 metadata uses native block references (`archive_session` + `block_id`). Only
/// selected regions read PCM; only each session's own chunks are decoded.
pub fn metadata(archive: &Path, output: &Path) -> Result<()> {
    let archive = archive.canonicalize()?;
    heimdall_archive::private_dir(output)?;
    storage::require(output, 65536)?;
    let mut evidence = std::fs::File::create(output.join("evidence.jsonl"))?;
    let mut manifest = std::fs::File::create(output.join("manifest.jsonl"))?;
    let mut audio = std::fs::File::create(output.join("audio.jsonl"))?;
    let mut sources = std::fs::File::create(output.join("sources.jsonl"))?;
    let sessions = store::sessions(&archive)?;
    for session in &sessions {
        let view = heimdall_archive::View::session(&store::session_path(&archive, session))?;
        view.for_each_record(|r| {
            if r.payload["kind"] == "speaker_probabilities" {
                line(&mut evidence, output, json!({"session":session,"record":{"source":r.source,"payload":r.payload}}))?;
            }
            Ok(())
        })?;
        view.transcript()?;
        let mut q = view.db.prepare("SELECT id,source_id,start_ns,end_ns FROM temp.utterances ORDER BY start_ns,id")?;
        for r in q.query_map([], |r| Ok(json!({"session":session,"utterance_id":r.get::<_,String>(0)?,"source_id":r.get::<_,String>(1)?,"start_ns":r.get::<_,i64>(2)?,"end_ns":r.get::<_,i64>(3)?})))? {
            line(&mut manifest, output, r?)?;
        }
    }
    storage::atomic_write(
        &output.join("dataset.json"),
        &serde_json::to_vec(&json!({"schema":3,"archive_root":archive,"night_native_blocks":true}))?,
    )?;
    let discovery = rusqlite::Connection::open_in_memory()?;
    let flags = crate::dataset::regions(output, &discovery)?;
    let mut uninspected = vec![];
    for session in &sessions {
        let windows: Vec<_> = flags.iter().filter(|r| r.session == *session).collect();
        if windows.is_empty() {
            uninspected.push(session.clone());
            continue;
        }
        let view = heimdall_archive::View::session(&store::session_path(&archive, session))?;
        view.capture()?;
        let mut q = view.db.prepare("SELECT metadata_json FROM temp.sources ORDER BY segment,id")?;
        for r in q.query_map([], |r| r.get::<_, String>(0))? {
            line(&mut sources, output, json!({"session":session,"metadata":serde_json::from_str::<Value>(&r?)?}))?;
        }
        let mut q = view.db.prepare("SELECT id,source_id,host_ns,rate,channels,frames,discontinuity FROM temp.blocks ORDER BY source_id,host_ns,id")?;
        for r in q.query_map([], |r| {
            let start = r.get::<_, i64>(2)?;
            let rate = r.get::<_, u32>(3)?;
            let frames = r.get::<_, i64>(5)?;
            Ok(json!({"session":session,"data":{"archive_session":session,"block_id":r.get::<_,i64>(0)?,"source":r.get::<_,String>(1)?,"start_ns":start,"end_ns":start+frames*1_000_000_000/rate as i64,"rate":rate,"channels":r.get::<_,u16>(4)?,"discontinuity":r.get::<_,bool>(6)?}}))
        })? {
            let row = r?;
            let segment = &row["data"];
            if windows.iter().any(|w| {
                segment["source"] == w.source
                    && segment["start_ns"].as_u64().unwrap() < w.end_ns.saturating_add(1_000_000_000)
                    && segment["end_ns"].as_u64().unwrap() > w.start_ns.saturating_sub(1_000_000_000)
            }) {
                line(&mut audio, output, row)?;
            }
        }
    }
    storage::atomic_write(
        &output.join("dataset.json"),
        &serde_json::to_vec(&json!({"schema":3,"archive_root":archive,"night_native_blocks":true,"uninspected_source_inventory":uninspected}))?,
    )?;
    Ok(())
}
/// The shared lifetime lock pins blobs; the block is read live or from its chunk and decoded
/// in RAM only for the intersecting interval.
pub fn native_block(dataset: &Path, segment: &Value, a: u64, b: u64) -> Result<(Vec<f32>, u32, u16)> {
    let meta: Value = serde_json::from_slice(&std::fs::read(dataset.join("dataset.json"))?)?;
    let archive = PathBuf::from(meta["archive_root"].as_str().context("night archive root")?).canonicalize()?;
    ensure!(segment["archive_session"].is_string(), "native block reference");
    let _reader = heimdall_archive::compact::reader_lock(&archive)?;
    let db = store::open(&archive)?;
    let pcm = heimdall_archive::audio::read_block(&db, segment["block_id"].as_i64().context("native block")?)?;
    let start = segment["start_ns"].as_u64().context("block start")?;
    let first = (((a - start) * pcm.rate as u64 + 500_000_000) / 1_000_000_000) as usize * pcm.channels as usize;
    let last = (((b - start) * pcm.rate as u64 + 500_000_000) / 1_000_000_000) as usize * pcm.channels as usize;
    ensure!(last <= pcm.samples.len(), "native block changed or truncated");
    Ok((pcm.samples[first..last].to_vec(), pcm.rate, pcm.channels))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discover_before_decode_ignores_unrelated_pcm_and_preserves_originals() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let session = store::session_path(&root, "session-test");
        let journal = heimdall_archive::Journal::open(&session).unwrap();
        journal.event("model_output","call",&[],1_000_000_000,json!({"kind":"speaker_probabilities","revision":1,"origin_seconds":1.,"output":{"seconds_per_frame":0.5,"changed_frames":[{"frame":0,"probabilities":[0.9,0.9]}]}})).unwrap();
        let db = store::open(&root).unwrap();
        let segment = store::create_segment(&db, journal.session_id(), "capture-test", 1_000_000_000, 0, "native").unwrap();
        db.execute("INSERT INTO sources VALUES(?1,'call',?2,0,NULL)", rusqlite::params![segment, json!({"id":"call"}).to_string()]).unwrap();
        let pcm = vec![0.25; 48000];
        let object = heimdall_archive::audio::put_hot(&db, 16000, 1, &pcm).unwrap();
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'call',0,16000,1,48000,'f32-zstd',0,?2,0,1)", rusqlite::params![segment, object]).unwrap();
        // An unrelated source whose audio is never decoded (its object does not even exist).
        db.execute("INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'unrelated',0,16000,1,999999,'f32-zstd',0,'missing',0,1)", [segment]).unwrap();
        // A session with no flagged window is listed as uninspected, its capture never read.
        heimdall_archive::Journal::open(&store::session_path(&root, "session-unflagged")).unwrap();
        let data = t.path().join("scratch");
        metadata(&root, &data).unwrap();
        assert!(!data.join("audio").exists());
        assert_eq!(std::fs::read_to_string(data.join("audio.jsonl")).unwrap().lines().count(), 1);
        let meta: Value = serde_json::from_str(&std::fs::read_to_string(data.join("dataset.json")).unwrap()).unwrap();
        assert_eq!(meta["uninspected_source_inventory"], json!(["session-unflagged"]));
        let mem = rusqlite::Connection::open_in_memory().unwrap();
        let regions = crate::dataset::regions(&data, &mem).unwrap();
        assert_eq!(regions.len(), 1);
        let (decoded, start) = crate::dataset::read_region(&data, &regions[0]).unwrap();
        assert_eq!(start, 0);
        assert_eq!(decoded.len(), 40000);
        assert!(decoded.iter().all(|v| *v == 0.25));
        // The night pass only reads the store.
        assert_eq!(db.query_row("SELECT count(*) FROM blocks", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    }
}

/// Read one call window off the app thread when the live PCM ring has expired.
/// Shared native refs and the same region decoder preserve gaps and one clock.
pub fn call_region(session:&Path, region:&crate::dataset::Region) -> Result<(Vec<f32>,u64)> {
    let archive=session.parent().context("call archive root")?;
    let _pin=pin(archive)?;
    let view=heimdall_archive::View::session(session)?;
    let a=region.start_ns.saturating_sub(1_000_000_000);let b=region.end_ns+1_000_000_000;
    view.capture_span(&region.source,a,b)?;
    let mut query=view.db.prepare("SELECT id,host_ns,rate,channels,frames,discontinuity FROM temp.blocks WHERE source_id=?1 AND host_ns<?3 AND host_ns+frames*1000000000/rate>?2 ORDER BY host_ns,id")?;
    let refs=query.query_map(rusqlite::params![region.source,a as i64,b as i64],|r| {
        let start=r.get::<_,i64>(1)?;let rate=r.get::<_,u32>(2)?;let frames=r.get::<_,i64>(4)?;
        Ok(json!({"archive_session":region.session,"block_id":r.get::<_,i64>(0)?,"start_ns":start,"end_ns":start+frames*1_000_000_000/rate as i64,"rate":rate,"channels":r.get::<_,u16>(3)?,"discontinuity":r.get::<_,bool>(5)?}))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let temporary=tempfile::tempdir()?;
    std::fs::write(temporary.path().join("dataset.json"),serde_json::to_vec(&json!({"archive_root":archive}))?)?;
    let mut region=region.clone();region.audio=refs;
    crate::dataset::read_region(temporary.path(),&region)
}
