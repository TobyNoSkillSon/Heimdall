//! Latest diarization revisions -> bounded overlap jobs. Never infer identity from slot order.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Region {
    pub session: String,
    pub source: String,
    pub start_ns: u64,
    pub end_ns: u64,
    pub speakers: usize,
    pub utterance_ids: Vec<String>,
    pub audio: Vec<Value>,
}
pub fn each_line(path: &Path, mut f: impl FnMut(Value) -> Result<()>) -> Result<()> {
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        ensure!(line.len() <= 32 * 1024 * 1024, "dataset line too large");
        f(serde_json::from_str(&line)?)?;
    }
    Ok(())
}
pub fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    ensure!(
        Path::new(relative)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_))),
        "unsafe dataset path"
    );
    let path = root.join(relative).canonicalize()?;
    ensure!(
        path.starts_with(root.canonicalize()?),
        "dataset symlink escape"
    );
    Ok(path)
}
pub fn regions(dataset: &Path, db: &Connection) -> Result<Vec<Region>> {
    let meta: Value = serde_json::from_slice(&std::fs::read(dataset.join("dataset.json"))?)?;
    ensure!(
        meta["schema"].as_u64() == Some(3),
        "night requires archive store dataset schema 3"
    );
    db.execute_batch("DROP TABLE IF EXISTS frames; CREATE TABLE frames(session TEXT,source TEXT,origin INTEGER,frame INTEGER,start INTEGER,end INTEGER,speakers INTEGER,revision INTEGER,PRIMARY KEY(session,source,origin,frame));
 DROP TABLE IF EXISTS utterances; CREATE TABLE utterances(session TEXT,source TEXT,id TEXT,start INTEGER,end INTEGER); CREATE INDEX u_time ON utterances(session,source,start,end);
 DROP TABLE IF EXISTS audio; CREATE TABLE audio(session TEXT,source TEXT,start INTEGER,end INTEGER,data TEXT); CREATE INDEX a_time ON audio(session,source,start,end);")?;
    each_line(&dataset.join("evidence.jsonl"), |row| {
        let p = &row["record"]["payload"];
        if p["kind"] != "speaker_probabilities" {
            return Ok(());
        }
        let session = row["session"].as_str().context("speaker session")?;
        let source = row["record"]["source"].as_str().context("speaker source")?;
        let origin = p["origin_seconds"].as_f64().context("speaker origin")?;
        let step = p["output"]["seconds_per_frame"]
            .as_f64()
            .context("speaker frame clock")?;
        ensure!(
            origin.is_finite() && origin >= 0. && step.is_finite() && step > 0. && step <= 1.,
            "invalid speaker clock"
        );
        let revision = p["revision"].as_i64().context("speaker revision")?;
        for frame in p["output"]["changed_frames"]
            .as_array()
            .context("changed speaker frames")?
        {
            let n = frame["frame"].as_i64().context("frame")?;
            ensure!(n >= 0, "negative frame");
            let probabilities = frame["probabilities"].as_array().context("probabilities")?;
            ensure!(
                probabilities.len() <= 8
                    && probabilities.iter().all(|v| v
                        .as_f64()
                        .is_some_and(|p| p.is_finite() && (0.0..=1.).contains(&p))),
                "bad speaker scores"
            );
            let speakers = probabilities
                .iter()
                .filter(|v| v.as_f64().unwrap() >= 0.5)
                .count();
            let a = ((origin + n as f64 * step) * 1e9).round() as i64;
            let b = ((origin + (n + 1) as f64 * step) * 1e9).round() as i64;
            db.execute("INSERT INTO frames VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(session,source,origin,frame) DO UPDATE SET start=excluded.start,end=excluded.end,speakers=excluded.speakers,revision=excluded.revision WHERE excluded.revision>=frames.revision",params![session,source,(origin*1e9).round() as i64,n,a,b,speakers as i64,revision])?;
        }
        Ok(())
    })?;
    each_line(&dataset.join("manifest.jsonl"), |r| {
        db.execute(
            "INSERT INTO utterances VALUES(?1,?2,?3,?4,?5)",
            params![
                r["session"].as_str(),
                r["source_id"].as_str(),
                r["utterance_id"].as_str(),
                r["start_ns"].as_u64().unwrap_or(0) as i64,
                r["end_ns"].as_u64().unwrap_or(0) as i64
            ],
        )?;
        Ok(())
    })?;
    each_line(&dataset.join("audio.jsonl"), |r| {
        let a = &r["data"];
        db.execute(
            "INSERT INTO audio VALUES(?1,?2,?3,?4,?5)",
            params![
                r["session"].as_str(),
                a["source"].as_str(),
                a["start_ns"].as_u64().unwrap_or(0) as i64,
                a["end_ns"].as_u64().unwrap_or(0) as i64,
                serde_json::to_string(a)?
            ],
        )?;
        Ok(())
    })?;
    let mut intervals: Vec<Region> = vec![];
    let mut q=db.prepare("SELECT session,source,start,end,speakers FROM frames WHERE speakers>=2 ORDER BY session,source,start,end")?;
    for row in q.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? as u64,
            r.get::<_, i64>(3)? as u64,
            r.get::<_, i64>(4)? as usize,
        ))
    })? {
        let (session, source, a, b, n) = row?;
        if let Some(last) = intervals.last_mut().filter(|p| {
            p.session == session && p.source == source && a <= p.end_ns && p.speakers == n
        }) {
            last.end_ns = last.end_ns.max(b);
        } else {
            intervals.push(Region {
                session,
                source,
                start_ns: a,
                end_ns: b,
                speakers: n,
                utterance_ids: vec![],
                audio: vec![],
            });
        }
    }
    // At most six seconds owned by a job; one second of context on each side fits the 8s model cap.
    let mut jobs = vec![];
    for interval in intervals {
        let mut a = interval.start_ns;
        while a < interval.end_ns {
            let b = (a + 6_000_000_000).min(interval.end_ns);
            let mut region = Region {
                start_ns: a,
                end_ns: b,
                ..interval.clone()
            };
            let mut q=db.prepare("SELECT DISTINCT id FROM utterances WHERE session=?1 AND source=?2 AND start<?4 AND end>?3 ORDER BY id")?;
            region.utterance_ids = q
                .query_map(
                    params![region.session, region.source, a as i64, b as i64],
                    |r| r.get(0),
                )?
                .collect::<rusqlite::Result<_>>()?;
            let mut q=db.prepare("SELECT data FROM audio WHERE session=?1 AND source=?2 AND start<?4 AND end>?3 ORDER BY start")?;
            let data = q
                .query_map(
                    params![
                        region.session,
                        region.source,
                        a.saturating_sub(1_000_000_000) as i64,
                        (b + 1_000_000_000) as i64
                    ],
                    |r| r.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            region.audio = data
                .iter()
                .map(|s| serde_json::from_str(s))
                .collect::<serde_json::Result<_>>()?;
            jobs.push(region);
            a = b;
        }
    }
    Ok(jobs)
}
/// Decode only intersecting samples; callback gaps are a failure, never padded as invented speech.
pub fn read_region(dataset: &Path, region: &Region) -> Result<(Vec<f32>, u64)> {
    let start = region.start_ns.saturating_sub(1_000_000_000).max(
        region
            .audio
            .first()
            .and_then(|a| a["start_ns"].as_u64())
            .unwrap_or(region.start_ns),
    );
    let end = (region.end_ns + 1_000_000_000).min(
        region
            .audio
            .last()
            .and_then(|a| a["end_ns"].as_u64())
            .unwrap_or(region.end_ns),
    );
    ensure!(
        start <= region.start_ns && end >= region.end_ns && end > start,
        "overlap audio missing"
    );
    let mut output = vec![];
    let mut cursor = start;
    let mut geometry = None;
    for segment in &region.audio {
        let a = segment["start_ns"]
            .as_u64()
            .context("audio start")?
            .max(start);
        let b = segment["end_ns"].as_u64().context("audio end")?.min(end);
        if b <= a {
            continue;
        }
        ensure!(a.abs_diff(cursor) <= 125000, "capture gap in night input");
        ensure!(
            !segment["discontinuity"].as_bool().unwrap_or(false) || a == start,
            "discontinuous night input"
        );
        let rate = segment["rate"]
            .as_u64()
            .or(segment["native_rate"].as_u64())
            .context("audio rate")? as u32;
        let channels = segment["channels"]
            .as_u64()
            .or(segment["native_channels"].as_u64())
            .context("audio channels")? as u16;
        let segment_start=segment["start_ns"].as_u64().context("audio start")?;
        let first_frame=((a-segment_start)*rate as u64+500_000_000)/1_000_000_000;
        let last_frame=((b-segment_start)*rate as u64+500_000_000)/1_000_000_000;
        let frames=(last_frame-first_frame) as usize;
        let mut samples = if segment["archive_session"].is_string() {
            let (samples, native_rate, native_channels) = crate::intake::native_block(dataset, segment, a, b)?;
            ensure!((rate, channels) == (native_rate, native_channels), "native audio geometry changed");
            samples
        } else if segment["silence"] == true {
            vec![0.; frames * channels as usize]
        } else {
            let path = safe_path(
                dataset,
                segment["audio"].as_str().context("audio reference")?,
            )?;
            let mut wav = hound::WavReader::open(path)?;
            let spec = wav.spec();
            ensure!(
                spec.sample_rate == rate && spec.channels == channels,
                "audio geometry mismatch"
            );
            let offset = segment["offset_samples"].as_u64().unwrap_or(0)
                + first_frame;
            wav.seek(u32::try_from(offset)?)?;
            let count = frames * channels as usize;
            let samples: Vec<f32> = match spec.sample_format {
                hound::SampleFormat::Float => wav
                    .samples::<f32>()
                    .take(count)
                    .collect::<std::result::Result<_, _>>()?,
                hound::SampleFormat::Int => wav
                    .samples::<i32>()
                    .take(count)
                    .map(|v| v.map(|x| x as f32 / 2_f32.powi(spec.bits_per_sample as i32 - 1)))
                    .collect::<std::result::Result<_, _>>()?,
            };
            ensure!(samples.len() == count, "truncated archived audio");
            samples
        };
        let gain = segment["gain"].as_f64().unwrap_or(1.) as f32;
        ensure!(gain.is_finite(), "invalid archive gain");
        for value in &mut samples {
            *value *= gain;
        }
        ensure!(
            samples.iter().all(|v| v.is_finite()),
            "nonfinite archive samples"
        );
        ensure!(geometry.is_none_or(|g| g == (rate, channels)), "audio geometry changed within region");
        geometry = Some((rate, channels));
        output.extend(samples);
        cursor = b;
    }
    let (rate, channels) = geometry.context("overlap has no PCM")?;
    // Resample once, retaining one filter/rounding clock across native callbacks.
    let output = heimdall_archive::audio::mono_16k(&output, rate, channels)?;
    ensure!(
        cursor.abs_diff(end) <= 125000
            && output.len() >= 8000
            && output.len() <= crate::separator::MAX_SAMPLES,
        "missing/too short overlap audio"
    );
    Ok((output, start))
}

/// Coverage is explicit: archived diarization is the detector; missing model
/// history never masquerades as "no overlap". Call identities match app routing.
pub fn coverage(dataset: &Path, db: &Connection) -> Result<Value> {
    let mut calls = std::collections::BTreeSet::new();
    if dataset.join("sources.jsonl").is_file() {
        each_line(&dataset.join("sources.jsonl"), |row| {
            let meta = &row["metadata"];
            let process = &meta["kind"]["Process"];
            let bundle = process["owner"]["bundle_id"]
                .as_str()
                .or(process["bundle_id"].as_str());
            if bundle.is_some_and(|s| {
                [
                    "com.hnc.Discord",
                    "com.hnc.DiscordPTB",
                    "com.hnc.DiscordCanary",
                    "com.tinyspeck.slackmacgap",
                    "us.zoom.xos",
                    "com.microsoft.teams2",
                    "com.microsoft.teams",
                    "com.apple.FaceTime",
                ]
                .contains(&s)
            }) {
                calls.insert((
                    row["session"].as_str().context("session")?.to_owned(),
                    meta["id"].as_str().context("source id")?.to_owned(),
                ));
            }
            Ok(())
        })?;
    }
    let mut query=db.prepare("SELECT session,source,min(start),max(end),count(*) FROM frames GROUP BY session,source ORDER BY session,source")?;
    let rows = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let missing: Vec<_> = calls
        .iter()
        .filter(|(s, id)| !rows.iter().any(|r| &r.0 == s && &r.1 == id))
        .map(|(s, id)| json!({"session":s,"source":id}))
        .collect();
    let covered: Vec<_> = rows
        .iter()
        .map(|(s, id, a, b, n)| json!({"session":s,"source":id,"start_ns":a,"end_ns":b,"frames":n}))
        .collect();
    let meta: Value = serde_json::from_slice(&std::fs::read(dataset.join("dataset.json"))?)?;
    let uninspected = meta["uninspected_source_inventory"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok(
        json!({"detector":"latest archived Nemotron speaker_probabilities; threshold 0.5, at least two active tracks","covered_sources":covered,"call_sources_without_model_history":missing,"source_inventory_not_scanned":uninspected,"complete":missing.is_empty() && uninspected.is_empty(),"caveat":"frame coverage is model history, not proof that every real overlap was detected"}),
    )
}
