use crate::{audio, hash, private_dir, store, View};
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{io::Write, path::Path};

#[path = "export_join.rs"]
mod joins;

/// Training export retains model events and source timelines; no cloud or models run.
/// Audio is emitted once per unique PCM object, with utterance interval references.
pub fn export(root: &Path, output: &Path) -> Result<usize> {
    export_with_additional(root, output, None)
}

/// Join optional offline outputs without changing any original archive record.
pub fn export_with_additional(
    root: &Path,
    output: &Path,
    additional: Option<&Path>,
) -> Result<usize> {
    ensure!(!output.exists(), "export destination already exists");
    ensure!(
        !output.starts_with(root),
        "export must be outside the archive"
    );
    ensure!(store::exists(root), "no archive store at {}", root.display());
    private_dir(output)?;
    private_dir(&output.join("audio"))?;
    private_dir(&output.join("images"))?;
    let mut manifest = std::fs::File::create(output.join("manifest.jsonl"))?;
    let mut events = std::fs::File::create(output.join("evidence.jsonl"))?;
    let mut timeline = std::fs::File::create(output.join("audio.jsonl"))?;
    let mut sources = std::fs::File::create(output.join("sources.jsonl"))?;
    let mut provenance = std::fs::File::create(output.join("blocks.jsonl"))?;
    let mut vectors = std::fs::File::create(output.join("vectors.jsonl"))?;
    let mut image_index = std::fs::File::create(output.join("images.jsonl"))?;
    let mut count = 0;
    for session in store::sessions(root)? {
        let view = View::session(&store::session_path(root, &session))?;
        view.capture()?;
        view.transcript()?;
        let db = &view.db;
        let mut blocks = vec![];
        let mut pending: Option<HotGroup> = None;
        let mut emitted: std::collections::HashMap<String, (String, u32, u16)> =
            std::collections::HashMap::new();
        let mut source_rows =
            db.prepare("SELECT metadata_json,added_ns,removed_ns FROM temp.sources ORDER BY segment,id")?;
        for row in source_rows.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?))
        })? {
            let (metadata, a, b) = row?;
            writeln!(
                sources,
                "{}",
                json!({"session":session,"metadata":serde_json::from_str::<Value>(&metadata)?,"added_ns":a,"removed_ns":b})
            )?;
        }
        let mut stmt = db.prepare("SELECT id,source_id,host_ns,rate,channels,frames,discontinuity,codec,object_id,offset_frames,gain FROM temp.blocks ORDER BY source_id,host_ns,id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u64,
                r.get::<_, u32>(3)?,
                r.get::<_, u16>(4)?,
                r.get::<_, i64>(5)? as usize,
                r.get::<_, bool>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, i64>(9)? as usize,
                r.get::<_, f64>(10)? as f32,
            ))
        })?;
        let mut cache = audio::ObjectCache::default();
        for row in rows {
            let (id, source, start, rate, channels, frames, gap, codec, object, offset, gain) = row?;
            let end = start + frames as u64 * 1_000_000_000 / rate as u64;
            writeln!(
                provenance,
                "{}",
                json!({"session":session,"block_id":id,"source":source,"host_ns":start,"native_rate":rate,"native_channels":channels,"frames":frames,"codec":codec,"discontinuity":gap})
            )?;
            if codec == "f32-zstd" {
                let pcm = audio::read_block_cached(db, id, &mut cache)?;
                let append = pending.as_ref().is_some_and(|p| {
                    p.source == source
                        && p.pcm.rate == pcm.rate
                        && p.pcm.channels == pcm.channels
                        && !gap
                        && p.end.abs_diff(start) <= 1_000_000_000 / rate as u64
                        && p.pcm.samples.len() + pcm.samples.len()
                            <= ((pcm.rate as usize * pcm.channels as usize * 30).min(2 * 1024 * 1024))
                });
                if !append {
                    if let Some(group) = pending.take() {
                        emit_hot(group, output, &session, &mut timeline, &mut blocks)?;
                    }
                }
                if let Some(group) = &mut pending {
                    group.pcm.samples.extend(pcm.samples);
                    group.end = end;
                } else {
                    pending = Some(HotGroup { source: source.clone(), start, end, gap, pcm });
                }
                continue;
            }
            if let Some(group) = pending.take() {
                emit_hot(group, output, &session, &mut timeline, &mut blocks)?;
            }
            let value = if codec == "zero" {
                json!({"source":source,"start_ns":start,"end_ns":end,"silence":true,"native_rate":rate,"native_channels":channels,"frames":frames,"discontinuity":gap})
            } else {
                let object = object.with_context(|| format!("block {id} has no object"))?;
                let (relative, decoded_rate, decoded_channels) = if let Some(value) = emitted.get(&object) {
                    value.clone()
                } else {
                    let pcm = audio::read_object(db, &object)?;
                    let mut bytes = pcm.rate.to_le_bytes().to_vec();
                    bytes.extend(pcm.channels.to_le_bytes());
                    bytes.extend(audio::pcm_bytes(&pcm.samples));
                    let key = hash(&bytes);
                    let relative = format!("audio/{key}.wav");
                    if !output.join(&relative).exists() {
                        let mut writer = hound::WavWriter::create(
                            output.join(&relative),
                            hound::WavSpec {
                                channels: pcm.channels,
                                sample_rate: pcm.rate,
                                bits_per_sample: 32,
                                sample_format: hound::SampleFormat::Float,
                            },
                        )?;
                        for x in &pcm.samples {
                            writer.write_sample(*x)?;
                        }
                        writer.finalize()?;
                    }
                    let value = (relative, pcm.rate, pcm.channels);
                    emitted.insert(object.clone(), value.clone());
                    value
                };
                json!({"source":source,"start_ns":start,"end_ns":end,"audio":relative,"object_id":object,"gain":gain,"offset_samples":offset as u64*decoded_rate as u64/rate as u64,"samples":(offset+frames) as u64*decoded_rate as u64/rate as u64-offset as u64*decoded_rate as u64/rate as u64,"rate":decoded_rate,"channels":decoded_channels,"native_rate":rate,"native_channels":channels,"discontinuity":gap,"tier":if codec=="cold-ref"{"cold"}else{"hot"}})
            };
            writeln!(timeline, "{}", json!({"session":session,"block_id":id,"data":value}))?;
            push_segment(&mut blocks, value);
        }
        if let Some(group) = pending.take() {
            emit_hot(group, output, &session, &mut timeline, &mut blocks)?;
        }
        let mut gaps = db.prepare("SELECT source_id,host_ns,detail FROM temp.gaps ORDER BY id")?;
        for row in gaps.query_map([],|r|Ok(json!({"session":session,"kind":"capture_gap","source":r.get::<_,Option<String>>(0)?,"host_ns":r.get::<_,i64>(1)?,"detail":r.get::<_,String>(2)?})))?{writeln!(events,"{}",row?)?;}
        // Read all evidence, including uncertain decisions and failed actions.
        view.for_each_record(|r| {
            writeln!(events, "{}", json!({"session":session,"record":r}))?;
            Ok(())
        })?;
        let mut stmt=db.prepare("SELECT id,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision FROM temp.utterances ORDER BY start_ns,id")?;
        for row in stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)? as u64,
                r.get::<_, i64>(5)? as u64,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
            ))
        })? {
            let (id, source, label, speaker, start, end, text, words, notes, decision) = row?;
            let audio: Vec<_> = blocks
                .iter()
                .filter(|b| {
                    b["source"] == source
                        && b["start_ns"].as_u64().unwrap() < end
                        && b["end_ns"].as_u64().unwrap() > start
                })
                .map(|b| clip_segment(b, start, end))
                .collect();
            let audio_gaps = audio_gaps(&audio, start, end);
            writeln!(
                manifest,
                "{}",
                json!({"schema":1,"session":session,"utterance_id":id,"source_id":source,"source":label,"speaker":speaker,"start_ns":start,"end_ns":end,"text":text,"words":serde_json::from_str::<Value>(&words)?,"notes":serde_json::from_str::<Value>(&notes)?,"decision":serde_json::from_str::<Value>(&decision)?,"audio_segments":audio,"audio_missing":audio.is_empty(),"audio_gaps":audio_gaps,"audio_complete":audio_gaps.is_empty(),"evidence":"evidence.jsonl"})
            )?;
            count += 1;
        }
    }
    // Vectors and screenshots are content-addressed across the whole store: each is emitted once.
    {
        let _reader = crate::compact::reader_lock(root)?;
        let db = store::open(root)?;
        let ids: Vec<String> = db.prepare("SELECT id FROM vectors ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for id in ids {
            writeln!(vectors, "{}", json!({"vector_id":id,"values":crate::records::vector_values(&db, &id)?}))?;
        }
        let ids: Vec<String> = db.prepare("SELECT id FROM images ORDER BY id")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        for id in ids {
            let (mime, bytes) = crate::records::image_bytes(&db, root, &id)?;
            let ext = match mime.as_str() {
                "image/webp" => "webp",
                "image/jpeg" => "jpg",
                _ => "png",
            };
            let relative = format!("images/{id}.{ext}");
            writeln!(image_index, "{}", json!({"image_id":id,"mime":mime,"file":relative,"sha256":hash(&bytes)}))?;
            std::fs::write(output.join(relative), bytes)?;
        }
    }
    if let Some(root) = additional {
        joins::additional(root, output, &mut events)?;
    }
    manifest.sync_all()?;
    events.sync_all()?;
    timeline.sync_all()?;
    sources.sync_all()?;
    provenance.sync_all()?;
    vectors.sync_all()?;
    image_index.sync_all()?;
    drop(manifest);
    drop(events);
    joins::join(output)?;
    std::fs::write(
        output.join("dataset.json"),
        serde_json::to_vec_pretty(
            &json!({"schema":3,"join":"manifest evidence_refs -> evidence event_id (session qualified); unlinked events remain in evidence.jsonl","utterances":count,"clock":"capture host nanoseconds; wall time separately in evidence","manifest":"manifest.jsonl","audio_timeline":"audio.jsonl","events":"evidence.jsonl","sources":"sources.jsonl","vectors":"vectors.jsonl (store-wide, once each)","images":"images.jsonl (store-wide, once each)","blocks":"blocks.jsonl","audio_encoding":"IEEE float32 WAV; original rate/channels in timeline; cold is explicitly marked"}),
        )?,
    )?;
    Ok(count)
}

struct HotGroup {
    source: String,
    start: u64,
    end: u64,
    gap: bool,
    pcm: audio::Pcm,
}
fn emit_hot(
    group: HotGroup,
    output: &Path,
    session: &str,
    timeline: &mut std::fs::File,
    segments: &mut Vec<Value>,
) -> Result<()> {
    let mut identity = group.pcm.rate.to_le_bytes().to_vec();
    identity.extend(group.pcm.channels.to_le_bytes());
    identity.extend(audio::pcm_bytes(&group.pcm.samples));
    let id = hash(&identity);
    let relative = format!("audio/{id}.wav");
    if !output.join(&relative).is_file() {
        let mut writer = hound::WavWriter::create(
            output.join(&relative),
            hound::WavSpec {
                channels: group.pcm.channels,
                sample_rate: group.pcm.rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )?;
        for x in &group.pcm.samples {
            writer.write_sample(*x)?;
        }
        writer.finalize()?;
    }
    let value = json!({"source":group.source,"start_ns":group.start,"end_ns":group.end,"audio":relative,"audio_id":id,"offset_samples":0,"samples":group.pcm.samples.len()/group.pcm.channels as usize,"rate":group.pcm.rate,"channels":group.pcm.channels,"gain":1.,"discontinuity":group.gap,"tier":"hot"});
    writeln!(timeline, "{}", json!({"session":session,"data":value}))?;
    push_segment(segments, value);
    Ok(())
}
fn push_segment(segments: &mut Vec<Value>, value: Value) {
    if let Some(previous) = segments.last_mut() {
        let continuous = value["audio"].is_string()
            && previous["source"] == value["source"]
            && previous["audio"] == value["audio"]
            && previous["gain"] == value["gain"]
            && previous["rate"] == value["rate"]
            && previous["channels"] == value["channels"]
            && !value["discontinuity"].as_bool().unwrap_or(false)
            && value["start_ns"]
                .as_u64()
                .unwrap()
                .abs_diff(previous["end_ns"].as_u64().unwrap())
                <= 125000;
        let adjacent = previous["offset_samples"]
            .as_u64()
            .zip(previous["samples"].as_u64())
            .zip(value["offset_samples"].as_u64())
            .is_some_and(|((o, n), next)| o + n == next);
        if continuous && adjacent {
            previous["samples"] =
                json!(previous["samples"].as_u64().unwrap() + value["samples"].as_u64().unwrap());
            previous["end_ns"] = value["end_ns"].clone();
            return;
        }
    }
    segments.push(value);
}

fn clip_segment(segment: &Value, start: u64, end: u64) -> Value {
    let mut out = segment.clone();
    let original = segment["start_ns"].as_u64().unwrap();
    let a = original.max(start);
    let b = segment["end_ns"].as_u64().unwrap().min(end);
    if let Some(rate) = segment["rate"].as_u64() {
        let delta = (a - original) * rate / 1_000_000_000;
        out["offset_samples"] = json!(segment["offset_samples"].as_u64().unwrap_or(0) + delta);
        out["samples"] = json!(((b - original) * rate / 1_000_000_000)
            .saturating_sub(delta)
            .min(
                segment["samples"]
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(delta)
            ));
    }
    out["start_ns"] = json!(a);
    out["end_ns"] = json!(b);
    out
}

fn audio_gaps(segments: &[Value], start: u64, end: u64) -> Vec<Value> {
    let mut intervals: Vec<_> = segments
        .iter()
        .map(|s| {
            (
                s["start_ns"].as_u64().unwrap(),
                s["end_ns"].as_u64().unwrap(),
            )
        })
        .collect();
    intervals.sort();
    let mut cursor = start;
    let mut gaps = vec![];
    for (a, b) in intervals {
        if a > cursor {
            gaps.push(json!({"start_ns":cursor,"end_ns":a}));
        }
        cursor = cursor.max(b);
    }
    if cursor < end {
        gaps.push(json!({"start_ns":cursor,"end_ns":end}));
    }
    gaps
}

/// An isolated, consistent copy of the whole store for offline export: the index and every
/// pack through SQLite's online backup (committed WAL included), under the shared lifetime lock so
/// no blob is collected meanwhile. The caller owns and deletes the copy.
pub fn snapshot_archive(source: &Path, destination: &Path) -> Result<()> {
    ensure!(!destination.exists(), "snapshot destination exists");
    ensure!(store::exists(source), "no archive store at {}", source.display());
    let parent = destination.parent().context("snapshot parent")?;
    private_dir(parent)?;
    ensure!(
        !parent.canonicalize()?.starts_with(source.canonicalize()?),
        "snapshot must be outside archive"
    );
    let _reader = crate::compact::reader_lock(source)?;
    let staging = tempfile::tempdir_in(parent)?;
    private_dir(&staging.path().join(store::PACKS))?;
    let copy = |from: &Path, to: &Path| -> Result<()> {
        let src = Connection::open_with_flags(from, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        src.busy_timeout(std::time::Duration::from_secs(10))?;
        let mut dst = Connection::open(to)?;
        rusqlite::backup::Backup::new(&src, &mut dst)?.run_to_completion(1024, std::time::Duration::ZERO, None)?;
        ensure!(
            dst.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0))? == "ok",
            "snapshot integrity"
        );
        Ok(())
    };
    copy(&source.join(store::INDEX), &staging.path().join(store::INDEX))?;
    for pack in store::packs(source)? {
        copy(&store::pack_path(source, &pack)?, &store::pack_path(staging.path(), &pack)?)?;
    }
    crate::atomic_write(
        &staging.path().join("NIGHT_SNAPSHOT"),
        &serde_json::to_vec(
            &json!({"schema":2,"scope":"archive store","consistency":"SQLite online backup per file, committed WAL included, under the shared lifetime lock"}),
        )?,
    )?;
    std::fs::rename(staging.keep(), destination)?;
    Ok(())
}
