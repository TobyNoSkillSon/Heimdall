//! Isolated diagnostics use the production projection and completion paths.
use super::*;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

pub(crate) fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let manifest = PathBuf::from(args.next().context("manifest")?);
    let models = PathBuf::from(args.next().context("models")?);
    let data = PathBuf::from(args.next().context("isolated data")?);
    let output = PathBuf::from(args.next().context("report")?);
    let first: usize = args.next().context("first")?.parse()?;
    let end: usize = args.next().context("end")?.parse()?;
    let rows: Vec<Value> = serde_json::from_slice(&std::fs::read(&manifest)?)?;
    anyhow::ensure!(
        !data.exists(),
        "replay requires a fresh isolated data directory"
    );
    anyhow::ensure!(
        !data.starts_with(std::env::var("HOME")?.to_owned() + "/Library/Application Support"),
        "live profile prohibited"
    );
    let mut p = Pipeline::new(
        data.clone(),
        models.clone(),
        Settings::default(),
        None,
        true,
    )?;
    if args.next().as_deref() == Some("--attribution-only") {
        return attribution_only(&mut p, &rows[first..end], &output);
    }
    let store = speakers::identity::EnrollmentStore::load(
        &manifest.parent().unwrap().join("private/speakers.json"),
    )?;
    store.save(&data.join("speakers.json"))?;
    let live_models = models.clone();
    let worker = Arc::new(listen::ListenWorker::spawn(
        listen::QueueConfig::default(),
        move || {
            let asr =
                listen::asr::Parakeet::load_backend(&live_models.join("parakeet-ultra"), "metal")?;
            ListenEngine::load_with_turn_backend(&live_models, asr, Some("metal"))
        },
    )?);
    worker.wait_ready()?;
    p.live_worker = Some(worker.clone());
    let began = Instant::now();
    let swap = heimdall_night::correction::swap_bytes()?;
    let probe = &rows[first];
    let region = heimdall_night::dataset::Region {
        session: probe["id"]
            .as_str()
            .unwrap()
            .rsplit_once(':')
            .unwrap()
            .0
            .into(),
        source: probe["source_id"].as_str().unwrap().into(),
        start_ns: probe["owned_start_ns"].as_u64().unwrap(),
        end_ns: probe["owned_end_ns"].as_u64().unwrap(),
        speakers: 2,
        utterance_ids: vec![],
        audio: vec![],
    };
    let original_session = heimdall_archive::store::session_path(
        &manifest.parent().unwrap().join("private/archive"),
        &region.session,
    );
    let (probe_audio, _) = heimdall_night::intake::call_region(&original_session, &region)?;
    worker.transcribe_side(
        900000,
        0,
        probe_audio[..probe_audio.len().min(16000)].to_vec(),
    )?;
    while !matches!(
        worker.recv_event(),
        Some(listen::WorkerEvent::SideTranscript {
            id: 900000,
            result: Ok(_),
            ..
        })
    ) {}
    let mut command_waits = vec![];
    for attempt in 0..3u64 {
        let correction_id = 910000 + attempt;
        let command_id = 920000 + attempt;
        worker.transcribe_correction(correction_id, 0, probe_audio.clone())?;
        let waiting = Instant::now();
        while worker.progress().correction_started.is_none() {
            anyhow::ensure!(
                waiting.elapsed() < Duration::from_secs(3),
                "correction did not start"
            );
            std::thread::sleep(Duration::from_micros(200));
        }
        let at = Instant::now();
        worker.transcribe_side(
            command_id,
            10_000_000_000,
            probe_audio[..probe_audio.len().min(3200)].to_vec(),
        )?;
        loop {
            if let Some(listen::WorkerEvent::SideTranscript {
                id,
                result,
                decode_ms,
                ..
            }) = worker.recv_event()
            {
                anyhow::ensure!(
                    id == command_id,
                    "command queued inside correction did not precede its remaining slices"
                );
                result.map_err(anyhow::Error::msg)?;
                let total = at.elapsed().as_secs_f64() * 1000.;
                command_waits.push(json!({"command_wait_ms":(total-decode_ms).max(0.),"response_ms":total,"decode_ms":decode_ms,"queued_after_correction_started":true}));
                break;
            }
        }
        while !matches!(worker.recv_event(),Some(listen::WorkerEvent::SideTranscript {id,result:Ok(_),..}) if id==correction_id)
        {
        }
    }
    let mut cases = vec![];
    for index in first..end {
        anyhow::ensure!(
            began.elapsed() < Duration::from_secs(540),
            "replay wall bound"
        );
        let row = &rows[index];
        let source = row["source_id"].as_str().context("source")?;
        let original_id = row["id"].as_str().context("id")?;
        let id = index as u64 + 1;
        let observation_id = format!("{}:{id}", p.session.file_name().unwrap().to_string_lossy());
        let words: Vec<Word> = serde_json::from_value(row["words"].clone())?;
        let start = row["start_ns"].as_u64().context("start")?;
        let end = row["end_ns"].as_u64().context("end")?;
        let origin = row["origin_ns"].as_u64().context("origin")?;
        let segments = serde_json::from_value(row["segments"].clone())?;
        let observation = agent::protocol::Observation {
            id: observation_id.clone(),
            timestamp_ms: row["wall_ms"].as_u64().unwrap(),
            source: "Discord".into(),
            speaker: "Unknown".into(),
            language: row["language"].as_str().unwrap_or("unknown").into(),
            text: row["text"].as_str().unwrap().into(),
        };
        p.index.record(
            &observation,
            source,
            start,
            end,
            &words,
            &[],
            &json!({"status":"already_sent"}),
        )?;
        let seq = p.tools.send_context.lock().unwrap().observe_at(
            "Unknown",
            &observation.text,
            end,
            serde_json::to_value(&observation)?,
        );
        p.tools.send_context.lock().unwrap().sent(seq);
        p.speech_split.finals.insert(
            id,
            Final {
                source: source.into(),
                origin,
                start,
                end,
                words: words.clone(),
                notes: vec![],
                last: String::new(),
                projected: vec![],
                revision: 0,
            },
        );
        p.speech_split.timelines.insert(
            (source.into(), origin),
            Timeline {
                scanned: 0,
                segments,
                finalized: vec![],
            },
        );
        for name in row["names"].as_array().into_iter().flatten() {
            p.remember_call_name(
                &TrackId {
                    source: source.into(),
                    session: origin,
                    speaker: name["speaker"].as_i64().unwrap() as i32,
                },
                name["name"].as_str().unwrap(),
            );
        }
        p.project_call(source)?;
        let baseline = p.index.utterances_between(start, end)?;
        let key = format!("replay:{index}");
        let region = heimdall_night::dataset::Region {
            session: original_id.rsplit_once(':').unwrap().0.into(),
            source: source.into(),
            start_ns: row["owned_start_ns"].as_u64().unwrap(),
            end_ns: row["owned_end_ns"].as_u64().unwrap(),
            speakers: 2,
            utterance_ids: vec![original_id.into()],
            audio: vec![],
        };
        let original_session = heimdall_archive::store::session_path(
            &manifest.parent().unwrap().join("private/archive"),
            &region.session,
        );
        let (samples, start_ns) = heimdall_night::intake::call_region(&original_session, &region)?;
        let input_seconds = samples.len() as f64 / 16000.;
        let input = Input {
            region,
            start_ns,
            samples,
            enrollments: store.clone(),
        };
        // Use the actual app-owned CPU runtime and bounded side-ASR completion path.
        if p.speech_split.runtime.is_none() {
            p.speech_split.runtime = Some(crate::speech_split::Runtime::start(
                models.clone(),
                p.internal.0.clone(),
                p.session.clone(),
                data.join("night/calls"),
                p.tools.archive_floor.clone(),
            )?);
        }
        let lifecycle_source = format!("replay-lifecycle:{index}");
        let actor = crate::speakers_runtime::Actor::expired_replay_fixture(p.speaker_cpu.clone());
        p.discord.insert(
            lifecycle_source.clone(),
            Discord {
                origin,
                session: origin,
                actor_generation: origin,
                actor,
                speaking: false,
                fed_until: end,
                active: true,
                loading: false,
                closed: false,
                segments: vec![],
                unlabelled: vec![],
            },
        );
        p.speech_split
            .active
            .insert((lifecycle_source.clone(), origin));
        p.speech_split.outstanding += 1;
        p.speech_split.reservations.insert(key.clone(), JOB_BYTES);
        p.speech_split.reserved_bytes += JOB_BYTES;
        p.speech_split
            .job_calls
            .insert(key.clone(), (lifecycle_source.clone(), origin));
        let at = Instant::now();
        p.speech_split
            .runtime
            .as_ref()
            .unwrap()
            .submit(key.clone(), input, at)?;
        // Expire while the default-model correction is outstanding, then drain via
        // normal completion events. The lifecycle source has no extra overlap jobs.
        p.speaker_update(crate::speakers_runtime::Event {
            source: lifecycle_source.clone(),
            generation: origin,
            update: crate::speakers_runtime::Update::Expired {
                origin,
                segments: vec![],
                events: vec![],
            },
        })?;
        anyhow::ensure!(
            p.speech_split.outstanding == 1 && p.speech_split.active.is_empty(),
            "expiry did not preserve drain"
        );
        let mut helper_pid = 0;
        let internal = p.internal_events();
        while p.speech_split.reservations.contains_key(&key) {
            anyhow::ensure!(at.elapsed() < Duration::from_secs(180), "case deadline");
            if let Ok(event) = internal.recv_timeout(Duration::from_millis(10)) {
                p.internal(event)?;
            }
            while let Some(event) = worker.pop_event() {
                p.worker_event(event)?;
            }
            helper_pid = helper_pid.max(p.speech_split.runtime.as_ref().unwrap().helper_pid());
            p.correction_tick()?;
        }
        p.project_call(source)?;
        let after = p.index.utterances_between(start, end)?;
        anyhow::ensure!(baseline == after, "separator changed published transcript");
        let evidence = p.index.evidence.records()?;
        let completed = evidence
            .iter()
            .find(|e| e.kind == "speech_split_completed" && e.payload["job"] == key)
            .context("completion evidence")?;
        let lines: Vec<_> = p.speech_split.finals[&id]
            .projected
            .iter()
            .map(|r| json!({"speaker":r.speaker,"track":r.track,"text":r.text(),"words":r.words}))
            .collect();
        let service = at.elapsed().as_secs_f64();
        let drain = Instant::now();
        loop {
            p.correction_tick()?;
            if p.speech_split.runtime.as_ref().unwrap().helper_pid() == 0 {
                break;
            }
            anyhow::ensure!(
                drain.elapsed() < Duration::from_secs(5),
                "helper did not unload after expiry/drain"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        anyhow::ensure!(
            unsafe { libc::kill(helper_pid as i32, 0) } == -1,
            "helper not reaped"
        );
        p.discord.remove(&lifecycle_source);
        anyhow::ensure!(
            p.tools
                .send_context
                .lock()
                .unwrap()
                .snapshot()
                .pending
                .is_empty(),
            "sent text became pending"
        );
        anyhow::ensure!(
            p.command_candidates.is_empty() && p.decisions == 0,
            "replay triggered actions"
        );
        cases.push(json!({"index":index,"id":original_id,"input_seconds":input_seconds,"service_seconds":service,"transcript":lines,"gate_verdicts":completed.payload["tracks"],"expiry_drained_unloaded":true,"helper_reaped":true}));
        std::fs::write(
            &output,
            serde_json::to_vec_pretty(
                &json!({"cases":cases,"command_probes":command_waits,"swap_before_bytes":swap,"swap_after_bytes":heimdall_night::correction::swap_bytes()?,"wall_seconds":began.elapsed().as_secs_f64()}),
            )?,
        )?;
        let case = cases.last().unwrap();
        println!("window {index} / {original_id}");
        for line in case["transcript"].as_array().unwrap() {
            println!(
                "  {} [track {}]: {}",
                line["speaker"].as_str().unwrap(),
                line["track"]["speaker"],
                line["text"].as_str().unwrap()
            );
        }
        for gate in case["gate_verdicts"].as_array().unwrap() {
            println!(
                "  gate slot {}: would_publish={} reasons={} owned={}",
                gate["slot"], gate["would_publish"], gate["reasons"], gate["owned_text"]
            );
        }
        eprintln!("case {index}: expiry/drain complete; helper reaped");
        anyhow::ensure!(
            heimdall_night::correction::swap_bytes()? <= swap,
            "swap growth"
        );
    }
    p.live_worker.take();
    drop(p);
    Ok(())
}

/// Text-only replay of frozen ASR/diarization evidence; no model loads, capture,
/// separator work or connection to the original archive/profile.
fn attribution_only(p: &mut Pipeline, rows: &[Value], output: &std::path::Path) -> Result<()> {
    let mut cases = vec![];
    for (index, row) in rows.iter().enumerate() {
        let id = index as u64 + 1;
        let source = row["source_id"].as_str().context("source")?;
        let origin = row["origin_ns"].as_u64().context("origin")?;
        let start = row["start_ns"].as_u64().context("start")?;
        let end = row["end_ns"].as_u64().context("end")?;
        let words: Vec<Word> = serde_json::from_value(row["words"].clone())?;
        let observation_id = format!("{}:{id}", p.session.file_name().unwrap().to_string_lossy());
        let observation = agent::protocol::Observation {
            id: observation_id.clone(),
            timestamp_ms: row["wall_ms"].as_u64().unwrap(),
            source: "Discord".into(),
            speaker: row["speaker"].as_str().unwrap().into(),
            language: row["language"].as_str().unwrap_or("unknown").into(),
            text: row["text"].as_str().unwrap().into(),
        };
        p.index.record(
            &observation,
            source,
            start,
            end,
            &words,
            &[],
            &json!({"status":"already_sent"}),
        )?;
        let seq = p.tools.send_context.lock().unwrap().observe_at(
            &observation.speaker,
            &observation.text,
            end,
            serde_json::to_value(&observation)?,
        );
        p.tools.send_context.lock().unwrap().sent(seq);
        p.speech_split.finals.insert(
            id,
            Final {
                source: source.into(),
                origin,
                start,
                end,
                words: words.clone(),
                notes: vec![],
                last: String::new(),
                projected: vec![],
                revision: 0,
            },
        );
        p.speech_split.timelines.insert(
            (source.into(), origin),
            Timeline {
                scanned: 0,
                segments: serde_json::from_value(row["segments"].clone())?,
                finalized: vec![],
            },
        );
        for name in row["names"].as_array().into_iter().flatten() {
            p.remember_call_name(
                &TrackId {
                    source: source.into(),
                    session: origin,
                    speaker: name["speaker"].as_i64().unwrap() as i32,
                },
                name["name"].as_str().unwrap(),
            );
        }
        p.project_call(source)?;
        let final_ = &p.speech_split.finals[&id];
        let runs = &final_.projected;
        let projected_words: Vec<_> = runs.iter().flat_map(|r| r.words.clone()).collect();
        anyhow::ensure!(
            serde_json::to_value(&projected_words)? == serde_json::to_value(&words)?,
            "projection lost/reordered/changed words"
        );
        let archive: Vec<_> = p
            .index
            .utterances_between(start, end)?
            .into_iter()
            .filter(|r| {
                r["id"] == observation_id
                    || r["id"]
                        .as_str()
                        .unwrap()
                        .starts_with(&(observation_id.clone() + ":speech:"))
            })
            .collect();
        anyhow::ensure!(archive.len() == runs.len(), "archive line count differs");
        for (archived, run) in archive.iter().zip(runs) {
            anyhow::ensure!(
                archived["text"] == run.text() && archived["speaker"] == run.speaker,
                "archive differs from UI projection"
            );
        }
        let context = p.tools.send_context.lock().unwrap().snapshot();
        let turn = context
            .turns
            .iter()
            .find(|t| t.seq == seq)
            .context("context turn")?;
        let (speaker, text) = context_text(runs);
        anyhow::ensure!(
            turn.text == text && turn.speaker == speaker,
            "decider text differs from projection"
        );
        anyhow::ensure!(
            context.pending.is_empty() && p.command_candidates.is_empty() && p.decisions == 0,
            "attribution triggered an action"
        );
        let after: Vec<_> = runs
            .iter()
            .map(|r| json!({"speaker":r.speaker,"track":r.track,"text":r.text()}))
            .collect();
        println!(
            "{}\nBefore — {}: {}",
            row["id"].as_str().unwrap(),
            observation.speaker,
            observation.text
        );
        for run in runs {
            println!("After  — {}: {}", run.speaker, run.text());
        }
        cases.push(json!({"id":row["id"], "before":{"speaker":observation.speaker,"text":observation.text}, "after":after, "archive":archive, "context":{"speaker":speaker,"text":text}, "words_preserved":true}));
        // Each frozen final has its own historical evidence. Do not reproject
        // earlier cases using the next final's snapshot or evict conversation.
        p.speech_split.finals.clear();
    }
    let split = cases
        .iter()
        .filter(|c| c["after"].as_array().unwrap().len() > 1)
        .count();
    let lines: usize = cases
        .iter()
        .map(|c| c["after"].as_array().unwrap().len())
        .sum();
    std::fs::write(
        output,
        serde_json::to_vec_pretty(
            &json!({"cases":cases,"finals":rows.len(),"split_finals":split,"total_lines":lines,"mean_lines_per_final":lines as f64/rows.len() as f64,"mean_lines_per_split_final":if split==0 {0.}else{(lines-(rows.len()-split)) as f64/split as f64},"minimum_owned_words":MIN_RUN_WORDS,"minimum_exclusive_seconds":MIN_RUN_EXCLUSIVE_SECONDS}),
        )?,
    )?;
    Ok(())
}
