use heimdall_night::{dataset, gate, job};
use serde_json::json;
use std::{fs, process::Command, time::Duration};
#[test]
fn late_revision_cancels_overlap_and_unknown_threeway_is_explicit() {
    let t = tempfile::tempdir().unwrap();
    let root = t.path();
    fs::write(root.join("dataset.json"), r#"{"schema":3}"#).unwrap();
    let event = |revision, frame, probabilities| {
        json!({"session":"session-1","record":{"source":"Discord","payload":{"kind":"speaker_probabilities","revision":revision,"origin_seconds":1.,"output":{"seconds_per_frame":0.1,"changed_frames":[{"frame":frame,"probabilities":probabilities}]}}}}).to_string()
    };
    fs::write(
        root.join("evidence.jsonl"),
        [
            event(1, 0, json!([0.9, 0.8, 0.])),
            event(2, 0, json!([0.9, 0.2, 0.])),
            event(1, 0, json!([0.9, 0.8, 0.])),
            event(2, 1, json!([0.9, 0.8, 0.7])),
        ]
        .join("\n"),
    )
    .unwrap();
    fs::write(root.join("manifest.jsonl"),json!({"session":"session-1","source_id":"Discord","utterance_id":"u1","start_ns":1_000_000_000_u64,"end_ns":2_000_000_000_u64}).to_string()).unwrap();
    fs::write(root.join("audio.jsonl"), "").unwrap();
    let db = rusqlite::Connection::open_in_memory().unwrap();
    let regions = dataset::regions(root, &db).unwrap();
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].start_ns, 1_100_000_000);
    assert_eq!(regions[0].speakers, 3);
    assert_eq!(regions[0].utterance_ids, ["u1"]);
}
#[test]
fn gate_denial_never_launches_and_activity_kills_worker() {
    let t = tempfile::tempdir().unwrap();
    let marker = t.path().join("spawned");
    let mut cmd = Command::new("/usr/bin/touch");
    cmd.arg(&marker);
    assert!(job::supervise(&mut cmd, || Ok(false), 1_000_000, Duration::from_secs(1)).is_err());
    assert!(!marker.exists());
    let mut calls = 0;
    let mut cmd = Command::new("/bin/sleep");
    cmd.arg("20");
    let start = std::time::Instant::now();
    assert!(job::supervise(
        &mut cmd,
        || {
            calls += 1;
            Ok(calls == 1)
        },
        1_000_000,
        Duration::from_secs(5)
    )
    .is_err());
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(calls >= 2);
}
#[test]
fn watchdog_deadline_and_ram_ceiling_kill_child() {
    for (rss, deadline) in [
        (0, Duration::from_secs(5)),
        (1_000_000, Duration::from_millis(50)),
    ] {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("20");
        assert!(job::supervise(&mut cmd, || Ok(true), rss, deadline).is_err());
    }
}
#[test]
fn safe_paths_and_gaps_are_rejected() {
    let t = tempfile::tempdir().unwrap();
    assert!(dataset::safe_path(t.path(), "../outside").is_err());
    let region = dataset::Region {
        session: "s".into(),
        source: "call".into(),
        start_ns: 1,
        end_ns: 100_000_001,
        speakers: 2,
        utterance_ids: vec![],
        audio: vec![],
    };
    assert!(dataset::read_region(t.path(), &region).is_err());
    assert!(!gate::State {
        on_ac: true,
        idle_seconds: f64::INFINITY
    }
    .eligible());
}

#[test]
fn phase_receipts_are_idempotent_and_never_overwrite_original_timings() {
    let t = tempfile::tempdir().unwrap();
    let region = dataset::Region {
        session: "s".into(),
        source: "call".into(),
        start_ns: 100,
        end_ns: 200,
        speakers: 2,
        utterance_ids: vec!["u1".into()],
        audio: vec![],
    };
    let id = job::phase(
        t.path(),
        &region,
        "night_asr_output",
        json!({"text":"first","latency_seconds":1.0}),
    )
    .unwrap();
    assert_eq!(
        id,
        job::phase(
            t.path(),
            &region,
            "night_asr_output",
            json!({"text":"first","latency_seconds":2.0})
        )
        .unwrap()
    );
    assert_ne!(
        id,
        job::phase(
            t.path(),
            &region,
            "night_asr_output",
            json!({"text":"second","latency_seconds":1.0})
        )
        .unwrap()
    );
    let rows = fs::read_dir(t.path().join("phases"))
        .unwrap()
        .map(|e| {
            serde_json::from_slice::<serde_json::Value>(&fs::read(e.unwrap().path()).unwrap())
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().find(|r| r["event_id"] == id).unwrap()["record"]["payload"]["latency_seconds"],
        1.0
    );
}

#[test]
fn archive_object_gain_is_applied_before_model_input() {
    let t = tempfile::tempdir().unwrap();
    fs::create_dir(t.path().join("audio")).unwrap();
    let mut wav = hound::WavWriter::create(
        t.path().join("audio/scaled.wav"),
        hound::WavSpec {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .unwrap();
    for i in 0..32000 {
        wav.write_sample(if i % 2 == 0 { 0.125_f32 } else { -0.25_f32 })
            .unwrap();
    }
    wav.finalize().unwrap();
    let region = dataset::Region {
        session: "s".into(),
        source: "call".into(),
        start_ns: 0,
        end_ns: 2_000_000_000,
        speakers: 2,
        utterance_ids: vec![],
        audio: vec![
            json!({"audio":"audio/scaled.wav","source":"call","start_ns":0,"end_ns":2_000_000_000_u64,"rate":16000,"channels":1,"offset_samples":0,"samples":32000,"gain":2.0}),
        ],
    };
    let (pcm, start) = dataset::read_region(t.path(), &region).unwrap();
    assert_eq!(start, 0);
    assert_eq!(pcm.len(), 32000);
    assert_eq!(pcm[0], 0.25);
    assert_eq!(pcm[1], -0.5);
}

#[test]
fn callback_blocks_resample_with_one_continuous_region_clock() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path();
    let pcm:Vec<_>=(0..96009).map(|n|(n as f32*0.03).sin()*0.1).collect();
    let mut wav=hound::WavWriter::create(root.join("native.wav"),hound::WavSpec {channels:1,sample_rate:48000,bits_per_sample:32,sample_format:hound::SampleFormat::Float}).unwrap();
    for sample in &pcm {wav.write_sample(*sample).unwrap();}wav.finalize().unwrap();
    let mut audio=vec![];
    let mut cursor=0usize;
    while cursor<pcm.len() {
        let next=(cursor+479).min(pcm.len());
        audio.push(json!({"audio":"native.wav","start_ns":cursor as u64*1_000_000_000/48000,"end_ns":next as u64*1_000_000_000/48000,"offset_samples":cursor,"rate":48000,"channels":1}));cursor=next;
    }
    let region=dataset::Region {session:"s".into(),source:"call".into(),start_ns:1_000_000_000,end_ns:1_000_100_000,speakers:2,utterance_ids:vec![],audio};
    let (decoded,start)=dataset::read_region(root,&region).unwrap();
    assert_eq!(start,0);
    let expected_end=2_000_100_000u64;
    let expected_samples=((expected_end*48000+500_000_000)/1_000_000_000) as usize;
    assert_eq!(decoded.len(),expected_samples/3,"callback rounding must not accumulate");
    let joined=heimdall_archive::audio::mono_16k(&pcm[..expected_samples],48000,1).unwrap();
    assert_eq!(decoded,joined);
}

#[test]
fn call_cpu_helper_is_reaped_and_models_unload_after_drain() {
    use heimdall_night::correction::{Client,Input};
    let models=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models");
    if !models.join("night/model.int8.onnx").exists() {eprintln!("native CPU models unavailable");return;}
    let mut client=Client::start(std::path::Path::new(env!("CARGO_BIN_EXE_heimdall-night")),&models).unwrap();
    let pid=client.pid();
    let input=Input {region:dataset::Region {session:"s".into(),source:"call".into(),start_ns:0,end_ns:500_000_000,speakers:2,utterance_ids:vec![],audio:vec![]},start_ns:0,samples:vec![0.;8003],enrollments:speakers::identity::EnrollmentStore::uncalibrated()};
    let output=client.run(&input).unwrap();assert_eq!(output.tracks.len(),2);
    drop(client);
    assert_eq!(unsafe {libc::kill(pid as i32,0)},-1,"owned process and native allocations must be gone");
}

/// 20 Oct: Heimdall's own RSS reached 4.7 GiB during a Discord call. The helper counted
/// the parent against a 3.8 GiB combined cap, exited from its watchdog two seconds into
/// every request, and the client saw "truncated correction helper result" 65 times.
/// This holds 4.2 GiB of touched pages in the parent (the test process) and runs two
/// six-second regions on one helper.
#[test]
#[ignore = "reproduction: holds 4.2 GiB in this process and runs the CPU models for about 40 s"]
fn cpu_helper_survives_a_parent_above_the_old_combined_budget() {
    use heimdall_night::correction::{Client,Input};
    let models=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models");
    if !models.join("night/model.int8.onnx").exists() {eprintln!("native CPU models unavailable");return;}
    let ballast=std::hint::black_box(vec![1u8;4_200*1024*1024]);
    let mut client=Client::start(std::path::Path::new(env!("CARGO_BIN_EXE_heimdall-night")),&models).unwrap();
    let mut seed=1u32;
    let samples:Vec<f32>=(0..7*16000).map(|i| {seed=seed.wrapping_mul(1664525).wrapping_add(1013904223);let noise=(seed>>8) as f32/16777216.-0.5;0.2*noise+0.3*((i as f32)*0.07).sin()}).collect();
    let input=Input {region:dataset::Region {session:"s".into(),source:"call".into(),start_ns:1_000_000_000,end_ns:7_000_000_000,speakers:2,utterance_ids:vec![],audio:vec![]},start_ns:0,samples,enrollments:speakers::identity::EnrollmentStore::uncalibrated()};
    for _ in 0..2 {
        let output=client.run(&input).unwrap();
        assert_eq!(output.tracks.len(),2);
    }
    assert_eq!(ballast[ballast.len()-1],1);
}
