//! One isolated model worker at a time. Its immutable output is the durable checkpoint.
use crate::storage::atomic_write;
use crate::{
    dataset::{self, Region},
    gate,
    separator::{Separator, MODEL_ID},
};
use anyhow::{ensure, Context, Result};
use heimdall_archive::{hash, private_dir, wall_ns, Record};
use listen::Recognizer;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub dataset: PathBuf,
    pub models: PathBuf,
    pub enrollments: Option<PathBuf>,
    pub output: PathBuf,
    pub region: Region,
    pub key: String,
    pub configuration: String,
}
fn streaming_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut block = [0; 65536];
    loop {
        let n = file.read(&mut block)?;
        if n == 0 {
            break;
        }
        hash.update(&block[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn model_tree(path: &Path, base: &Path, hash: &mut Sha256) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(!metadata.file_type().is_symlink(), "model symlink");
    if metadata.is_dir() {
        let mut entries = std::fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for p in entries {
            model_tree(&p, base, hash)?;
        }
    } else {
        hash.update(path.strip_prefix(base)?.to_string_lossy().as_bytes());
        hash.update(streaming_hash(path)?.as_bytes());
    }
    Ok(())
}
pub fn configuration(models: &Path, enrollments: Option<&Path>) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(streaming_hash(&std::env::current_exe()?)?.as_bytes());
    digest.update(b"heimdall-night-v1;overlap-.5;context-1s;max-core-6s;parakeet-metal;redim-cpu");
    for p in [
        models.join("night/model.int8.onnx"),
        models.join("parakeet-ultra"),
        models.join("speakers/ReDimNet2B6.onnx"),
    ] {
        model_tree(&p, models, &mut digest)?;
    }
    if let Some(p) = enrollments {
        digest.update(streaming_hash(p)?.as_bytes());
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn key(region: &Region, dataset: &Path, configuration: &str) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(configuration.as_bytes());
    digest.update(serde_json::to_vec(region)?);
    if region
        .audio
        .iter()
        .any(|a| a["archive_session"].is_string())
        && region.speakers == 2
    {
        let (audio, _) = dataset::read_region(dataset, region)?;
        digest.update(heimdall_archive::audio::pcm_bytes(&audio));
    }
    for a in &region.audio {
        if let Some(relative) = a["audio"].as_str() {
            digest.update(streaming_hash(&dataset::safe_path(dataset, relative)?)?.as_bytes());
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn save_audio(root: &Path, audio: &[f32]) -> Result<(String, String)> {
    private_dir(&root.join("audio"))?;
    let mut bytes = std::io::Cursor::new(vec![]);
    {
        let mut wav = hound::WavWriter::new(
            &mut bytes,
            hound::WavSpec {
                channels: 1,
                sample_rate: 16000,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )?;
        for x in audio {
            wav.write_sample(*x)?;
        }
        wav.finalize()?;
    }
    let bytes = bytes.into_inner();
    let wav_sha = hash(&bytes);
    let mut identity = 16000_u32.to_le_bytes().to_vec();
    identity.extend(1_u16.to_le_bytes());
    identity.extend(heimdall_archive::audio::pcm_bytes(audio));
    let id = hash(&identity);
    let relative = format!("audio/{id}.wav");
    if !root.join(&relative).exists() {
        atomic_write(&root.join(&relative), &bytes)?;
    } else {
        ensure!(
            streaming_hash(&root.join(&relative))? == wav_sha,
            "night object corrupted"
        );
    }
    Ok((relative, wav_sha))
}
/// Immutable phase receipts survive a killed worker before the final job commit.
pub fn phase(output: &Path, region: &Region, kind: &str, payload: Value) -> Result<String> {
    let mut identity = payload.clone();
    if let Some(map) = identity.as_object_mut() {
        map.remove("latency_seconds");
    }
    let id = format!(
        "night-stage:{}",
        hash(&serde_json::to_vec(
            &json!({"kind":kind,"region":region,"payload":identity})
        )?)
    );
    let record = Record {
        kind: kind.into(),
        source: region.source.clone(),
        utterance_ids: region.utterance_ids.clone(),
        host_ns: region.start_ns,
        wall_ns: wall_ns(),
        payload,
    };
    let path = output
        .join("phases")
        .join(format!("{}.json", id.strip_prefix("night-stage:").unwrap()));
    if !path.exists() {
        atomic_write(
            &path,
            &serde_json::to_vec(&json!({"session":region.session,"event_id":id,"record":record}))?,
        )?;
    }
    Ok(id)
}
#[cfg(target_os = "macos")]
pub fn infer(
    audio: &[f32],
    start: u64,
    models: &Path,
    enrollments: Option<&Path>,
    output: &Path,
    region: &Region,
    configuration: &str,
) -> Result<Value> {
    use speakers::embedding::VoiceEncoder;
    let owned = (region.start_ns, region.end_ns);
    let input_hash = hash(&heimdall_archive::audio::pcm_bytes(audio));
    let total = Instant::now();
    let load = Instant::now();
    let mut separator = Separator::load(&models.join("night/model.int8.onnx"))?;
    let separator_load = load.elapsed().as_secs_f64();
    let time = Instant::now();
    let tracks = separator.separate(audio)?;
    let separation_seconds = time.elapsed().as_secs_f64();
    drop(separator);
    let objects = tracks
        .iter()
        .map(|track| save_audio(output, track))
        .collect::<Result<Vec<_>>>()?;
    let track_refs:Vec<_>=objects.iter().enumerate().map(|(i,(relative,sha))|json!({"slot":i,"audio":relative,"audio_sha256":sha,"rate":16000,"start_ns":start,"end_ns":start+audio.len() as u64*62500})).collect();
    let separator_event = phase(
        output,
        region,
        "night_separator_output",
        json!({"configuration_sha256":configuration,"input_pcm_sha256":input_hash,"input":region,"model_id":MODEL_ID,"latency_seconds":separation_seconds,"tracks":track_refs}),
    )?;
    let load = Instant::now();
    let mut asr = listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), "metal")?;
    let asr_load = load.elapsed().as_secs_f64();
    let time = Instant::now();
    let mut transcripts = vec![];
    let mut transcript_events = vec![];
    for (i, track) in tracks.iter().enumerate() {
        let invocation = Instant::now();
        let transcript = asr.transcribe(track, start)?;
        transcript_events.push(phase(output,region,"night_asr_output",json!({"configuration_sha256":configuration,"model_id":"moondream/parakeet-ultra","backend":asr.backend(),"input_separator_event":separator_event,"input_pcm_sha256":input_hash,"latency_seconds":invocation.elapsed().as_secs_f64(),"tracks":[{"slot":i,"audio":objects[i].0,"audio_sha256":objects[i].1,"window_transcript":transcript}]}))?);
        transcripts.push(transcript);
    }
    let asr_seconds = time.elapsed().as_secs_f64();
    let asr_backend = asr.backend().to_owned();
    drop(asr);
    let store = if let Some(p) = enrollments {
        speakers::identity::EnrollmentStore::load(p)?
    } else {
        speakers::identity::EnrollmentStore::uncalibrated()
    };
    // Pin the existing CPU identity path. No naming popup or enrollment mutation.
    let time = Instant::now();
    let mut encoder =
        speakers::embedding::ReDimNet::load(&models.join("speakers/ReDimNet2B6.onnx"))?;
    let mut results = vec![];
    for (i, (track, transcript)) in tracks.iter().zip(transcripts).enumerate() {
        let embedding = if track.len() >= 32000 {
            encoder.embed(track)?
        } else {
            encoder.embed_short(track)?
        };
        let identity = if track.len() >= 32000 {
            serde_json::to_value(store.identify(&embedding))?
        } else {
            json!({"Unknown":{"reason":"short_context_uncalibrated"}})
        };
        let (relative, id) = &objects[i];
        let identity_event = phase(
            output,
            region,
            "night_identity_output",
            json!({"configuration_sha256":configuration,"input_separator_event":separator_event,"model_id":speakers::embedding::MODEL_ID,"slot":i,"audio":relative,"embedding":embedding.values(),"identity":identity}),
        )?;
        let owned_words: Vec<_> = transcript
            .words
            .iter()
            .filter(|w| {
                let middle = w.start_ns + (w.end_ns - w.start_ns) / 2;
                middle >= owned.0 && middle < owned.1
            })
            .cloned()
            .collect();
        let owned_transcript = listen::Transcript::from_words(owned_words);
        results.push(json!({"slot":i,"audio":relative,"audio_sha256":id,"rate":16000,"start_ns":start,"end_ns":start+track.len() as u64*62500,"identity":identity,"embedding":embedding.values(),"window_transcript":transcript,"owned_transcript":owned_transcript,"owned_start_ns":owned.0,"owned_end_ns":owned.1,"separator_event_id":separator_event,"transcript_event_id":transcript_events[i],"identity_event_id":identity_event}));
    }
    Ok(
        json!({"schema":1,"pipeline_version":"night-v1","separator":MODEL_ID,"asr":"moondream/parakeet-ultra","asr_backend":asr_backend,"identity_model":speakers::embedding::MODEL_ID,"tracks":results,"timings_seconds":{"separator_load":separator_load,"separation":separation_seconds,"asr_load":asr_load,"asr":asr_seconds,"identity_and_save":time.elapsed().as_secs_f64(),"worker_total":total.elapsed().as_secs_f64()},"input_audio_seconds":audio.len() as f64/16000.}),
    )
}
#[cfg(not(target_os = "macos"))]
pub fn infer(
    _: &[f32],
    _: u64,
    _: &Path,
    _: Option<&Path>,
    _: &Path,
    _: &Region,
    _: &str,
) -> Result<Value> {
    anyhow::bail!("night re-transcription requires macOS");
}

pub fn worker(request: &Path) -> Result<()> {
    gate::background()?;
    // A killed coordinator cannot leave its model child running indefinitely.
    let _watchdog = RuntimeGuard::start(Duration::from_secs(180))?;
    let request: Request = serde_json::from_slice(&std::fs::read(request)?)?;
    let (audio, start) = dataset::read_region(&request.dataset, &request.region)?;
    let mut result = infer(
        &audio,
        start,
        &request.models,
        request.enrollments.as_deref(),
        &request.output,
        &request.region,
        &request.configuration,
    )?;
    result["job_key"] = json!(request.key);
    result["configuration_sha256"] = json!(request.configuration);
    result["input"] = json!(request.region);
    // Raw tensors/transcripts live once in phase records; the completion is a projection.
    for track in result["tracks"].as_array_mut().into_iter().flatten() {
        track.as_object_mut().unwrap().remove("window_transcript");
        track.as_object_mut().unwrap().remove("embedding");
        track.as_object_mut().unwrap().remove("identity");
    }
    atomic_write(
        &request.output.join("pending-result.json"),
        &serde_json::to_vec(&result)?,
    )?;
    Ok(())
}
pub struct RuntimeGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl RuntimeGuard {
    pub fn start(deadline: Duration) -> Result<Self> {
        gate::background()?;
        ensure!(gate::state()?.eligible(), "worker gate denied");
        let parent = unsafe { libc::getppid() };
        let started = Instant::now();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = stop.clone();
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(Duration::from_secs(2));
                if done.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let eligible = gate::state().map(|s| s.eligible()).unwrap_or(false);
                let rss = gate::bounded_output(Command::new("/bin/ps").args([
                    "-o",
                    "rss=",
                    "-p",
                    &std::process::id().to_string(),
                ]))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok());
                if started.elapsed() >= deadline
                    || unsafe { libc::getppid() } != parent
                    || !eligible
                    || rss.is_none_or(|n| n > 4 * 1024 * 1024)
                {
                    // No pending result is ever committed by this abrupt exit.
                    unsafe { libc::_exit(75) };
                }
            }
        });
        Ok(Self(stop))
    }
}
impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Resource check sampled every 250ms; idle/power every two seconds. Caller injects the gate for tests.
pub fn supervise(
    command: &mut Command,
    mut eligible: impl FnMut() -> Result<bool>,
    max_rss_kib: u64,
    deadline: Duration,
) -> Result<()> {
    ensure!(eligible()?, "night deferred: user active or on battery");
    let mut child = command
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    let start = Instant::now();
    let mut probe = Instant::now() - Duration::from_secs(2);
    let outcome = (|| -> Result<()> {
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "night worker exited {status}");
                return Ok(());
            }
            ensure!(start.elapsed() < deadline, "night worker deadline");
            if probe.elapsed() >= Duration::from_secs(2) {
                ensure!(eligible()?, "night paused: user active or power removed");
                let rss = gate::bounded_output(Command::new("/bin/ps").args([
                    "-o",
                    "rss=",
                    "-p",
                    &child.id().to_string(),
                ]))?;
                ensure!(
                    rss.trim().parse::<u64>()? <= max_rss_kib,
                    "night worker RAM ceiling"
                );
                probe = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    })();
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    outcome
}
fn checkpoint(db: &Connection, key: &str, session: &str, record: &Record) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO jobs VALUES(?1,?2,?3)",
        params![key, session, serde_json::to_string(record)?],
    )?;
    Ok(())
}
fn completed(db: &Connection, key: &str, output: &Path) -> Result<bool> {
    let record = db
        .query_row(
            "SELECT record FROM jobs WHERE key=?1 AND record IS NOT NULL",
            [key],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(record) = record else {
        return Ok(false);
    };
    let record: Record = serde_json::from_str(&record)?;
    for track in record.payload["tracks"].as_array().into_iter().flatten() {
        let path =
            dataset::safe_path(output, track["audio"].as_str().context("checkpoint audio")?)?;
        ensure!(
            Some(streaming_hash(&path)?.as_str()) == track["audio_sha256"].as_str(),
            "corrupt checkpoint audio"
        );
        for field in [
            "separator_event_id",
            "transcript_event_id",
            "identity_event_id",
        ] {
            if let Some(id) = track[field].as_str() {
                let id = id
                    .strip_prefix("night-stage:")
                    .context("phase receipt id")?;
                let phase: Value = serde_json::from_slice(&std::fs::read(
                    output.join("phases").join(format!("{id}.json")),
                )?)?;
                ensure!(
                    phase["event_id"] == track[field],
                    "corrupt phase checkpoint"
                );
            }
        }
    }
    Ok(true)
}
fn rebuild_evidence(db: &Connection, output: &Path) -> Result<()> {
    let temp = output.join("evidence.jsonl.part");
    let mut file = File::create(&temp)?;
    let mut query = db.prepare(
        "SELECT key,session,record FROM jobs WHERE record IS NOT NULL ORDER BY session,key",
    )?;
    for row in query.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (key, session, record) = row?;
        let record: Value = serde_json::from_str(&record)?;
        crate::storage::require(
            output,
            record.to_string().len() as u64 + session.len() as u64 + key.len() as u64 + 256,
        )?;
        writeln!(
            file,
            "{}",
            json!({"session":session,"event_id":format!("night:{key}"),"record":record})
        )?;
    }
    heimdall_archive::full_sync(&file)?;
    drop(file);
    std::fs::rename(temp, output.join("evidence.jsonl"))?;
    Ok(())
}
#[derive(Debug, Serialize)]
pub struct RunOutcome {
    pub completed: usize,
    pub pending: bool,
}
impl std::fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} pending={}", self.completed, self.pending)
    }
}
pub fn run(
    dataset: &Path,
    models: &Path,
    enrollments: Option<&Path>,
    output: &Path,
) -> Result<RunOutcome> {
    ensure!(
        gate::state()?.eligible(),
        "night deferred: requires five minutes idle on AC power"
    );
    let _runtime_guard = RuntimeGuard::start(Duration::from_secs(590))?;
    let dataset = dataset.canonicalize()?;
    let output_parent = output.parent().context("night output parent")?;
    private_dir(output_parent)?;
    private_dir(output)?;
    let output = output.canonicalize()?;
    ensure!(
        !output.starts_with(&dataset) && !dataset.starts_with(&output),
        "night outputs must be outside input dataset"
    );
    // OS advisory lock, released on SIGKILL. No stale lock lease can strand a job.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.join(".night.lock"))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "night job already running"
        );
    }
    crate::storage::require(&output, 1024 * 1024)?;
    let db = Connection::open(output.join("jobs.sqlite"))?;
    crate::storage::limit_db(&db, &output)?;
    db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;PRAGMA cache_size=-4096;PRAGMA temp_store=MEMORY;CREATE TABLE IF NOT EXISTS jobs(key TEXT PRIMARY KEY,session TEXT,record TEXT);")?;
    // The byte-hashed configuration is part of every checkpoint. Changed weights/enrollments create a new version.
    let configuration = configuration(models, enrollments)?;
    let discovery = Connection::open_in_memory()?;
    let regions = dataset::regions(&dataset, &discovery)?;
    atomic_write(
        &output.join("coverage.json"),
        &serde_json::to_vec_pretty(&dataset::coverage(&dataset, &discovery)?)?,
    )?;
    rebuild_evidence(&db, &output)?;
    if let Some(region) = regions.first() {
        phase(
            &output,
            region,
            "night_admission",
            json!({"policy":"idle","admitted":true,"regions":regions.len()}),
        )?;
    }
    let mut done = 0;
    let mut pending_work = false;
    let run_start = Instant::now();
    for region in regions {
        if run_start.elapsed() >= Duration::from_secs(570) {
            pending_work = true;
            break;
        }
        ensure!(
            gate::state()?.eligible(),
            "night paused: user active or on battery"
        );
        // Failed PCM reads have a durable metadata key; successful jobs retain
        // their original byte-hashed identity and immutable phase validation.
        let failed_key = hash(&serde_json::to_vec(&json!([configuration, region]))?);
        if completed(&db, &failed_key, &output)? {
            continue;
        }
        let key = match self::key(&region, &dataset, &configuration) {
            Ok(key) => key,
            Err(error) => {
                persist_region(
                    &db,
                    &output,
                    &failed_key,
                    &configuration,
                    &region,
                    Err(error),
                )?;
                done += 1;
                continue;
            }
        };
        match completed(&db, &key, &output) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                persist_region(
                    &db,
                    &output,
                    &failed_key,
                    &configuration,
                    &region,
                    Err(error),
                )?;
                done += 1;
                continue;
            }
        }
        let result = (|| -> Result<Record> {
            Ok(if region.speakers != 2 {
                Record {
                    kind: "night_overlap_unsupported".into(),
                    source: region.source.clone(),
                    utterance_ids: region.utterance_ids.clone(),
                    host_ns: region.start_ns,
                    wall_ns: wall_ns(),
                    payload: json!({"job_key":key,"configuration_sha256":configuration,"input":region,"reason":"two-speaker model; three-or-more overlap retained without separation"}),
                }
            } else {
                let request = Request {
                    dataset: dataset.clone(),
                    models: models.canonicalize()?,
                    enrollments: enrollments.map(Path::canonicalize).transpose()?,
                    output: output.clone(),
                    region: region.clone(),
                    key: key.clone(),
                    configuration: configuration.clone(),
                };
                let request_path = output.join("pending-request.json");
                atomic_write(&request_path, &serde_json::to_vec(&request)?)?;
                let pending = output.join("pending-result.json");
                if pending.exists() {
                    std::fs::remove_file(&pending)?;
                }
                let mut command = Command::new(std::env::current_exe()?);
                command
                    .arg("worker")
                    .arg(&request_path)
                    .env("HEIMDALL_SPEAKERS_METAL", "0");
                supervise(
                    &mut command,
                    || Ok(gate::state()?.eligible()),
                    4 * 1024 * 1024,
                    Duration::from_secs(180)
                        .min(Duration::from_secs(590).saturating_sub(run_start.elapsed())),
                )?;
                let result: Value = serde_json::from_slice(&std::fs::read(&pending)?)?;
                ensure!(result["job_key"] == key, "wrong worker result");
                Record {
                    kind: "night_model_output".into(),
                    source: region.source.clone(),
                    utterance_ids: region.utterance_ids.clone(),
                    host_ns: region.start_ns,
                    wall_ns: wall_ns(),
                    payload: result,
                }
            })
        })();
        persist_region(&db, &output, &key, &configuration, &region, result)?;
        done += 1;
    }
    for name in ["pending-request.json", "pending-result.json"] {
        let path = output.join(name);
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }
    atomic_write(
        &output.join("progress.json"),
        &serde_json::to_vec(
            &json!({"completed":done,"pending":pending_work,"drained":!pending_work}),
        )?,
    )?;
    Ok(RunOutcome {
        completed: done,
        pending: pending_work,
    })
}

fn persist_region(
    db: &Connection,
    output: &Path,
    key: &str,
    configuration: &str,
    region: &Region,
    result: Result<Record>,
) -> Result<()> {
    let record=result.unwrap_or_else(|error| Record {
        kind:"night_region_failed".into(),source:region.source.clone(),utterance_ids:region.utterance_ids.clone(),host_ns:region.start_ns,wall_ns:wall_ns(),
        payload:json!({"job_key":key,"configuration_sha256":configuration,"input":region,"reason":format!("{error:#}")}),
    });
    crate::storage::limit_db(db, output)?;
    checkpoint(db, key, &region.session, &record)?;
    rebuild_evidence(db, output)
}

/// Snapshot/export and checkpoint path shared by the app and agent CLI.
pub fn process(
    archive: &Path,
    models: &Path,
    output: &Path,
    enrollments: Option<&Path>,
) -> Result<RunOutcome> {
    let _guard = RuntimeGuard::start(Duration::from_secs(590))?;
    heimdall_archive::private_dir(output)?;
    let archive_root = archive.canonicalize()?;
    let output_root = output.canonicalize()?;
    ensure!(
        !output_root.starts_with(&archive_root) && !archive_root.starts_with(&output_root),
        "night outputs must be outside input archive"
    );
    crate::storage::cleanup(output)?;
    let _lock = crate::storage::scratch_lock(output)?;
    crate::storage::require(output, 1024 * 1024)?;
    let temporary = tempfile::Builder::new()
        .prefix("night-snapshot-")
        .tempdir_in(output)?;
    heimdall_archive::atomic_write(
        &temporary.path().join("OWNER"),
        b"heimdall-night-scratch-v1",
    )?;
    let dataset = temporary.path().join("dataset");
    let _readers = crate::intake::pin(archive)?;
    crate::intake::metadata(archive, &dataset)?;
    // Dataset is nested in output; put durable results in a separate child.
    run(&dataset, models, enrollments, &output.join("results"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db(path: &Path) -> Connection {
        let db = Connection::open(path.join("jobs.sqlite")).unwrap();
        db.execute_batch("PRAGMA journal_mode=WAL;PRAGMA synchronous=FULL;CREATE TABLE IF NOT EXISTS jobs(key TEXT PRIMARY KEY,session TEXT,record TEXT)").unwrap();
        db
    }
    #[test]
    fn durable_failed_region_is_skipped_without_blocking_the_next_region() {
        let temp = tempfile::tempdir().unwrap();
        let db = db(temp.path());
        let region = Region {
            session: "s".into(),
            source: "call".into(),
            start_ns: 0,
            end_ns: 1,
            speakers: 2,
            utterance_ids: vec![],
            audio: vec![],
        };
        persist_region(
            &db,
            temp.path(),
            "failed",
            "config",
            &region,
            Err(anyhow::anyhow!("invalid PCM")),
        )
        .unwrap();
        assert!(completed(&db, "failed", temp.path()).unwrap());
        persist_region(
            &db,
            temp.path(),
            "later",
            "config",
            &region,
            Ok(Record {
                kind: "night_model_output".into(),
                source: "call".into(),
                utterance_ids: vec![],
                host_ns: 2,
                wall_ns: 2,
                payload: json!({"tracks":[]}),
            }),
        )
        .unwrap();
        assert!(completed(&db, "later", temp.path()).unwrap());
        let records = std::fs::read_to_string(temp.path().join("evidence.jsonl")).unwrap();
        assert_eq!(records.lines().count(), 2);
        assert!(records.contains("night_region_failed"));
        assert!(records.contains("night_model_output"));
        let pending = RunOutcome {
            completed: 2,
            pending: true,
        };
        let drained = RunOutcome {
            completed: 2,
            pending: false,
        };
        assert_ne!(pending.to_string(), drained.to_string());
    }
    #[test]
    #[ignore = "helper launched by abrupt_exit_resume test"]
    fn checkpoint_writer_helper() {
        let path = PathBuf::from(std::env::var("HEIMDALL_NIGHT_CHECKPOINT_TEST").unwrap());
        let db = db(&path);
        let (audio, sha) = save_audio(&path, &[0.1, 0.2, -0.4]).unwrap();
        checkpoint(
            &db,
            "job1",
            "session1",
            &Record {
                kind: "night_model_output".into(),
                source: "call".into(),
                utterance_ids: vec!["u1".into()],
                host_ns: 100,
                wall_ns: 200,
                payload: json!({"tracks":[{"audio":audio,"audio_sha256":sha}],"text":"first"}),
            },
        )
        .unwrap();
        // A committed WAL survives abrupt exit with no Drop/close/checkpoint cleanup.
        unsafe { libc::_exit(86) };
    }
    #[test]
    fn abrupt_exit_resume_validates_and_deduplicates_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "job::tests::checkpoint_writer_helper",
                "--ignored",
            ])
            .env("HEIMDALL_NIGHT_CHECKPOINT_TEST", temp.path())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86));
        let db = db(temp.path());
        assert!(completed(&db, "job1", temp.path()).unwrap());
        checkpoint(
            &db,
            "job1",
            "session1",
            &Record {
                kind: "night_model_output".into(),
                source: "call".into(),
                utterance_ids: vec![],
                host_ns: 0,
                wall_ns: 0,
                payload: json!({"text":"replacement"}),
            },
        )
        .unwrap();
        let record: String = db
            .query_row("SELECT record FROM jobs WHERE key='job1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&record).unwrap()["payload"]["text"],
            "first"
        );
        rebuild_evidence(&db, temp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("evidence.jsonl"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        let audio = std::fs::read_dir(temp.path().join("audio"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        std::fs::write(audio, b"corrupt").unwrap();
        assert!(completed(&db, "job1", temp.path()).is_err());
    }
}
