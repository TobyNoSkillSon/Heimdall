//! Explicit lab diagnostic: no hardware streams, prompts, profile or provider sends.
use crate::{night_runtime::Scheduler, pipeline::Pipeline, settings::Settings};
use anyhow::{ensure, Result};
use serde_json::json;
use std::{
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub fn worker(data: &Path, models: &Path, wav: &Path) -> Result<()> {
    heimdall_night::gate::background()?;
    std::env::set_var("HEIMDALL_SPEAKERS_METAL", "0");
    let parent = unsafe { libc::getppid() };
    std::thread::spawn(move || {
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if start.elapsed() > Duration::from_secs(150) || unsafe { libc::getppid() } != parent {
                unsafe { libc::_exit(75) }
            }
        }
    });
    let audio = crate::read_wav(wav)?;
    ensure!(
        audio.len() <= heimdall_night::separator::MAX_SAMPLES,
        "lab fixture at most eight seconds"
    );
    let region = heimdall_night::dataset::Region {
        session: "release-lab".into(),
        source: "public-mixture".into(),
        start_ns: 0,
        end_ns: audio.len() as u64 * 62500,
        speakers: 2,
        utterance_ids: vec![],
        audio: vec![],
    };
    let configuration = heimdall_night::job::configuration(models, None)?;
    let result =
        heimdall_night::job::infer(&audio, 0, models, None, data, &region, &configuration)?;
    crate::settings::private_write(
        &data.join("loaded.json"),
        &serde_json::to_vec_pretty(&result)?,
    )?;
    // Stay in actual separator inference so each preemption interrupts real work,
    // not a sleeping dummy. Completed immutable phases remain for inspection.
    let mut separator =
        heimdall_night::separator::Separator::load(&models.join("night/model.int8.onnx"))?;
    loop {
        std::hint::black_box(separator.separate(&audio)?);
    }
}

pub fn replay(data: PathBuf, models: PathBuf, wav: &Path) -> Result<()> {
    let gate = Arc::new(Mutex::new(heimdall_night::gate::State {
        on_ac: true,
        idle_seconds: 300.,
    }));
    let probe = gate.clone();
    let spawn_data = data.clone();
    let spawn_models = models.clone();
    // A committed public clip exercises the real models. No real capture grant is checked or requested.
    let night_wav = agent::runtime::checkout_root().join("models/night/public/item0_mix-16k.wav");
    ensure!(night_wav.is_file(), "public night fixture unavailable");
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let mut generation = 0;
    let scheduler = Scheduler::start_with(
        move || Ok(*probe.lock().unwrap()),
        move || {
            generation += 1;
            let output = spawn_data.join(format!("night-worker-{generation}"));
            crate::settings::private_dir(&output)?;
            let child = std::process::Command::new(std::env::current_exe()?)
                .arg("--night-lab-worker")
                .arg("--data-dir")
                .arg(&output)
                .arg("--models")
                .arg(&spawn_models)
                .arg("--replay")
                .arg(&night_wav)
                .process_group(0)
                .spawn()?;
            started_tx.send(output).unwrap();
            Ok(child)
        },
        Duration::ZERO,
    )?;
    let activity = scheduler.activity();
    let mut app = Pipeline::new(data.clone(), models, Settings::default(), None, true)?;
    app.install_lab_night(scheduler);
    let (tx, rx) = crossbeam_channel::bounded(512);
    app.preload(tx, false)?;
    let mut raw = app.raw_archive()?;
    let origin = capture::host_now_ns();
    let mut ids = vec![];
    for n in 0..3 {
        let info = crate::fake_source(if n == 0 { "mic" } else { "app" }, n, 1);
        raw.add_source(&info, origin)?;
        ids.push(info.id.clone());
        app.source(info);
    }
    let audio = crate::read_wav(wav)?;
    ensure!(!audio.is_empty(), "empty live fixture");
    app.night_admission(true); // No On/off changes anywhere in this diagnostic.
    let start = Instant::now();
    let mut tick = 0u64;
    let mut active_ticks = 0;
    let mut preemptions = vec![];
    let mut outputs = vec![];
    let mut stage = 0;
    let mut stage_start = Instant::now();
    let mut deadline = None;
    let mut speech_frames = 0;
    let mut real_speech_seen = false;
    let mut expected_pcm = Vec::new();
    // Synthetic idle/power/silence eligibility avoids waiting >5 minutes. Actual
    // live worker events still flow to the production scheduler Activity handle.
    loop {
        ensure!(
            start.elapsed() < Duration::from_secs(180),
            "night replay deadline"
        );
        while let Ok(output) = started_rx.try_recv() {
            outputs.push(output);
            stage_start = Instant::now();
        }
        let running = app.lab_night_running();
        if running {
            active_ticks += 1;
        }
        let loaded = outputs
            .last()
            .is_some_and(|p| p.join("loaded.json").is_file());
        let mut live_speech = false;
        match stage {
            0 if loaded => {
                stage = 1;
                stage_start = Instant::now();
                deadline = Some(Instant::now());
                live_speech = true;
            }
            1 => {
                live_speech = speech_frames < 300;
                if !running && deadline.is_some() {
                    // This latency includes WAV arrival -> actual Silero VAD -> process reap.
                    preemptions.push(json!({"trigger":"recorded_speech_first_packet","milliseconds":deadline.take().unwrap().elapsed().as_secs_f64()*1000.}));
                    real_speech_seen = true;
                }
                if speech_frames >= 300 {
                    stage = 2;
                    stage_start = Instant::now();
                }
            }
            2 if running
                && outputs.len() >= 2
                && stage_start.elapsed() > Duration::from_secs(1) =>
            {
                *gate.lock().unwrap() = heimdall_night::gate::State {
                    on_ac: true,
                    idle_seconds: 0.,
                };
                stage = 3;
                deadline = Some(Instant::now());
            }
            3 if !running => {
                preemptions.push(json!({"trigger":"input","milliseconds":deadline.take().unwrap().elapsed().as_secs_f64()*1000.}));
                *gate.lock().unwrap() = heimdall_night::gate::State {
                    on_ac: true,
                    idle_seconds: 300.,
                };
                stage = 4;
                stage_start = Instant::now();
            }
            4 if running
                && outputs.len() >= 3
                && stage_start.elapsed() > Duration::from_secs(1) =>
            {
                *gate.lock().unwrap() = heimdall_night::gate::State {
                    on_ac: false,
                    idle_seconds: 300.,
                };
                stage = 5;
                deadline = Some(Instant::now());
            }
            5 if !running => {
                preemptions.push(json!({"trigger":"unplug","milliseconds":deadline.take().unwrap().elapsed().as_secs_f64()*1000.}));
                *gate.lock().unwrap() = heimdall_night::gate::State {
                    on_ac: true,
                    idle_seconds: 300.,
                };
                stage = 6;
                stage_start = Instant::now();
            }
            6 if running
                && outputs.len() >= 4
                && stage_start.elapsed() > Duration::from_secs(1) =>
            {
                // Direct speech-start cancellation, excluding VAD's acoustic detection delay.
                deadline = Some(Instant::now());
                activity.speech("lab-event", true);
                stage = 7;
            }
            7 if !running => {
                preemptions.push(json!({"trigger":"speech_start_event","milliseconds":deadline.take().unwrap().elapsed().as_secs_f64()*1000.}));
                break;
            }
            _ => {}
        }
        let at = Duration::from_millis(tick * 20);
        if at > start.elapsed() {
            std::thread::sleep(at - start.elapsed());
        }
        let chunk = if live_speech {
            let offset = speech_frames * 320;
            speech_frames += 1;
            (0..320)
                .map(|i| audio[(offset + i) % audio.len()])
                .collect::<Vec<_>>()
        } else {
            vec![0.; 320]
        };
        expected_pcm.extend_from_slice(&chunk);
        let ns = origin + tick * 20_000_000;
        for id in &ids {
            raw.append(
                id,
                ns,
                capture::NativeFormat {
                    sample_rate: 16000,
                    channels: 1,
                },
                &chunk,
                false,
            )?;
            app.capture_event(capture::CaptureEvent::Audio(capture::AudioChunk {
                source_id: id.clone(),
                label: id.0.clone(),
                host_ns: ns,
                samples: chunk.clone(),
                discontinuity: false,
            }))?;
        }
        while let Ok(Some(event)) = rx.try_recv() {
            app.worker_event(event)?;
        }
        tick += 1;
    }
    app.night_admission(false);
    app.stop_live_worker(&rx)?;
    raw.flush()?;
    for id in &ids {
        app.finish_source(&id.0)?;
    }
    let gaps = app.gaps;
    let errors = app.errors;
    let session = app.session.clone();
    app.finish()?;
    let archive = capture::Archive::open_read_only(&session)?;
    let mut counts = vec![];
    let mut mismatches = 0usize;
    for id in &ids {
        let blocks = archive.read_blocks(id)?;
        counts.push(blocks.iter().map(|b| b.samples.len()).sum::<usize>());
        for b in blocks {
            ensure!(!b.discontinuity, "archive discontinuity");
        }
    }
    // Compare every archived float bit to the original replay input, including
    // silence and speech, so matching counts cannot hide replacement/reordering.
    for id in &ids {
        let actual = archive
            .read_blocks(id)?
            .into_iter()
            .flat_map(|b| b.samples)
            .collect::<Vec<_>>();
        mismatches += expected_pcm
            .iter()
            .zip(actual)
            .filter(|(a, b)| a.to_bits() != b.to_bits())
            .count();
    }
    let events = app.index.evidence.records()?;
    let finals = events
        .iter()
        .filter(|r| r.kind == "listen_event" && r.payload["event"] == "final_utterance")
        .count();
    let indexed = app.index.utterances_between(0, u64::MAX / 2)?;
    let per_stream = ids
        .iter()
        .map(|id| indexed.iter().filter(|r| r["source_id"] == id.0).count())
        .collect::<Vec<_>>();
    let receipt = json!({"listening_on":true,"hardware_capture":false,"injected_idle_power":true,"quiet_wait_seconds_lab":0,"production_quiet_wait_seconds":120,"busy_machine":true,"wall_seconds":start.elapsed().as_secs_f64(),"streams":3,"frames_per_stream":tick,"expected_samples_per_stream":tick*320,"archived_samples_per_stream":counts,"sample_mismatches_from_input":mismatches,"gaps":gaps,"errors":errors,"finals_expected":finals,"finals_indexed":indexed.len(),"finals_per_stream":per_stream,"night_active_audio_frames":active_ticks,"worker_starts":outputs.len(),"real_vad_preemption":real_speech_seen,"preemptions":preemptions,"session":session});
    crate::settings::private_write(
        &data.join("night-replay.json"),
        &serde_json::to_vec_pretty(&receipt)?,
    )?;
    println!("{receipt}");
    ensure!(
        counts == vec![(tick * 320) as usize; 3] && mismatches == 0 && gaps == 0 && errors == 0,
        "night replay audio loss"
    );
    ensure!(
        active_ticks > 0 && outputs.len() >= 4 && real_speech_seen,
        "night did not run"
    );
    ensure!(
        finals > 0 && finals == indexed.len() && per_stream.iter().all(|n| *n > 0),
        "live transcript loss"
    );
    Ok(())
}
