//! Resilience in the live loop (pipeline/resilience.rs, docs/resilience.md), through the real `Pipeline`: real
//! freezes told apart from quiet, the recovery ladder's rungs, the island's health cues, the microphone switch, the
//! decider's backoff, the disk, and the agent pre-flight. Models are absent or stubbed; nothing needs a permission.
use super::*;
use crate::health::Recovery;
use std::sync::mpsc;
use std::time::Duration;

fn app() -> (tempfile::TempDir, Pipeline) {
    let t = tempfile::tempdir().unwrap();
    let mut a = Pipeline::new(t.path().join("data"), t.path().join("missing-models"), Settings::default(), None, true).unwrap();
    // Live behaviour (no inline transcription of replayed audio).
    a.test_persist_settings();
    (t, a)
}
fn health(a: &Pipeline) -> Vec<ui::HealthCue> {
    a.hooks.ui.lock().unwrap().iter().filter_map(|u| match u { ui::UiUpdate::Health(c) => Some(c.clone()), _ => None }).collect()
}
fn audio(a: &mut Pipeline, info: &capture::SourceInfo, ns: u64) {
    a.capture_event(capture::CaptureEvent::Audio(capture::AudioChunk {
        source_id: info.id.clone(),
        label: info.label.clone(),
        host_ns: ns,
        // Digital silence: a quiet room still delivers samples.
        samples: vec![0.0; 320],
        discontinuity: false,
    }))
    .unwrap();
}
fn ago(seconds: u64) -> Instant {
    Instant::now() - Duration::from_secs(seconds)
}
const TICK: Duration = Duration::from_secs(2);

#[test]
fn a_quiet_room_is_never_a_stall_but_a_microphone_that_stops_delivering_climbs_the_ladder() {
    let (_t, mut a) = app();
    let (tx, _rx) = crossbeam_channel::bounded(8);
    a.listening_starting();
    let mic = crate::fake_source("mic", 0, 1);
    let tap = crate::fake_source("app", 1, 1);
    a.capture_event(capture::CaptureEvent::SourceAdded(mic.clone())).unwrap();
    a.capture_event(capture::CaptureEvent::SourceAdded(tap.clone())).unwrap();
    // Hours of silence: samples keep arriving, so the clock keeps restarting. The app tap delivers nothing at all
    // (its app is quiet), which is never a stall.
    for hour in 0..5u64 {
        a.mic_audio.insert(mic.id.0.clone(), ago(14));
        audio(&mut a, &mic, hour * 3_600_000_000_000);
        a.health_tick(TICK, &tx);
        assert_eq!(a.take_recovery(), None, "silence at hour {hour}");
    }
    // The loop itself was busy for 10 s: audio may be queued behind it, so capture is not judged this round.
    a.mic_audio.insert(mic.id.0.clone(), ago(16));
    a.health_tick(Duration::from_secs(10), &tx);
    assert_eq!(a.take_recovery(), None);
    // The microphone stops delivering for 16 s: restart capture, then the pipeline, then relaunch.
    let mut rungs = vec![];
    for _ in 0..3 {
        a.mic_audio.insert(mic.id.0.clone(), ago(16));
        a.health_tick(TICK, &tx);
        rungs.push(a.take_recovery().expect("a stall"));
    }
    assert!(matches!(rungs[0], Recovery::RestartCapture(_)), "{rungs:?}");
    assert!(matches!(rungs[1], Recovery::RestartPipeline(_)), "{rungs:?}");
    assert!(matches!(&rungs[2], Recovery::Relaunch { key, .. } if key == "capture"), "{rungs:?}");
    // Audio again: "Microphone OK" after the orange restart.
    audio(&mut a, &mic, 6 * 3_600_000_000_000);
    let cues = health(&a);
    assert!(matches!(&cues[0], ui::HealthCue::Restarting { key, label, .. } if key == "capture" && label == "Restarting the microphone…"), "{cues:?}");
    assert!(matches!(cues.last(), Some(ui::HealthCue::Recovered { key, label, .. }) if key == "capture" && label == "Microphone OK"), "{cues:?}");
    let stalls = a.index.evidence.records().unwrap().into_iter().filter(|r| r.kind == "capture_stalled").count();
    assert_eq!(stalls, 3);
    // A removed microphone (unplugged) is not watched; Listening off watches nothing.
    a.capture_event(capture::CaptureEvent::SourceRemoved { source_id: mic.id.clone(), host_ns: 10, reason: "input device disconnected".into() }).unwrap();
    a.health_tick(TICK, &tx);
    assert_eq!(a.take_recovery(), None);
    a.capture_event(capture::CaptureEvent::SourceAdded(mic.clone())).unwrap();
    a.set_running(false, "test").unwrap();
    a.health_tick(TICK, &tx);
    assert_eq!(a.take_recovery(), None);
}

/// Bug note 2026-10-27T16-57-38: a restarted capture's microphones come back under new ids while the stopped run's
/// removals arrive late (it stops off the live loop). The old clocks go, one stands in until a microphone of the new
/// run opens, a late removal of the old mic changes nothing, and a restart that brings no microphone back still
/// climbs the ladder.
#[test]
fn a_restarted_capture_is_watched_until_its_microphone_is_back_under_a_new_id() {
    let (_t, mut a) = app();
    let (tx, _rx) = crossbeam_channel::bounded(8);
    a.listening_starting();
    let old = crate::fake_source("mic", 0, 1);
    a.capture_event(capture::CaptureEvent::SourceAdded(old.clone())).unwrap();
    a.capture_restarting();
    let new = crate::fake_source("mic", 0, 2);
    a.capture_event(capture::CaptureEvent::SourceAdded(new.clone())).unwrap();
    a.capture_event(capture::CaptureEvent::SourceRemoved { source_id: old.id.clone(), host_ns: 10, reason: "capture stopped".into() }).unwrap();
    assert_eq!(a.mic_audio.keys().cloned().collect::<Vec<_>>(), vec![new.id.0.clone()]);
    audio(&mut a, &new, 20_000_000);
    a.health_tick(TICK, &tx);
    assert_eq!(a.take_recovery(), None, "the new run's microphone delivers");
    // A restart that opens no microphone at all: still a stall after 15 s.
    a.capture_restarting();
    a.capture_event(capture::CaptureEvent::SourceRemoved { source_id: new.id.clone(), host_ns: 30, reason: "capture stopped".into() }).unwrap();
    for t in a.mic_audio.values_mut() { *t = ago(16); }
    a.health_tick(TICK, &tx);
    assert!(matches!(a.take_recovery(), Some(Recovery::RestartCapture(_))), "a restart that brought no microphone back is a stall");
}

/// Bug note 2026-10-27T00-47-00: a capture health line names the input in use, so the log alone says the mic is live.
#[test]
fn capture_health_lines_name_the_input_in_use() {
    let (_t, mut a) = app();
    let (tx, _rx) = crossbeam_channel::bounded(8);
    a.listening_starting();
    let mic = crate::fake_source("mic", 0, 1);
    a.capture_event(capture::CaptureEvent::SourceAdded(mic.clone())).unwrap();
    assert_eq!(a.inputs_in_use(), format!("input: {}", mic.label));
    a.mic_audio.insert(mic.id.0.clone(), ago(16));
    a.health_tick(TICK, &tx);
    audio(&mut a, &mic, 20_000_000);
    a.evidence.flush().unwrap();
    let records: Vec<_> = a.index.evidence.records().unwrap().into_iter()
        .filter(|r| r.kind == "health" && r.payload["component"] == "capture").collect();
    assert!(records.len() >= 2, "{records:?}");
    for r in &records {
        assert_eq!(r.payload["reason"], format!("input: {}", mic.label), "{:?}", r.payload);
        let line = crate::diagnostics::evidence_line(r).map(|(l, _)| l);
        assert!(line.as_deref().is_some_and(|l| l.contains(&format!("(input: {})", mic.label))), "{line:?}");
    }
    a.capture_event(capture::CaptureEvent::SourceRemoved { source_id: mic.id.clone(), host_ns: 30, reason: "input device disconnected".into() }).unwrap();
    assert_eq!(a.inputs_in_use(), "input: none open");
}

/// A VAD that waits for a permit per call, so a test can hold the worker inside a model call.
struct Held(mpsc::Receiver<()>);
impl listen::SpeechDetector for Held {
    fn probability(&mut self, _: &[f32; 512], _: &mut listen::onnx::VadState) -> Result<f32> {
        let _ = self.0.recv();
        Ok(0.)
    }
}
struct Quiet;
impl listen::TurnDetector for Quiet {
    fn probability(&mut self, _: &[f32]) -> Result<f32> {
        Ok(0.)
    }
}
impl listen::Recognizer for Quiet {
    fn transcribe(&mut self, _: &[f32], _: u64) -> Result<listen::Transcript> {
        Ok(listen::Transcript::from_words(vec![]))
    }
}

#[test]
fn a_listen_worker_stuck_in_a_model_call_is_replaced_but_an_idle_one_never_is() {
    let (_t, mut a) = app();
    let (tx, _rx) = crossbeam_channel::bounded(512);
    a.listening_starting();
    let (permit, held) = mpsc::channel();
    let worker = Arc::new(listen::ListenWorker::spawn(Default::default(), move || ListenEngine::new(Quiet, Held(held), Quiet, Default::default())).unwrap());
    worker.wait_ready().unwrap();
    a.live_worker = Some(worker.clone());
    a.worker_spawned = Some(ago(600));
    // Idle for ten minutes: nothing waits, nothing is wrong.
    a.health_tick(TICK, &tx);
    a.worker_watch = a.worker_watch.map(|(d, _)| (d, ago(600)));
    a.health_tick(TICK, &tx);
    assert!(Arc::ptr_eq(a.live_worker.as_ref().unwrap(), &worker), "an idle worker was replaced");
    // Audio arrives; the worker goes into its first VAD call and stays there.
    for i in 0..4u64 {
        worker.push(listen::AudioChunk { source: "mic:test", generation: 1, host_time_ns: i * 32_000_000, samples: &[0.0; 512] }).unwrap();
    }
    a.health_tick(TICK, &tx);
    assert!(a.worker_watch.is_some(), "work waiting is watched");
    // 25 s without progress is still a slow decode, not a hang.
    a.worker_watch = a.worker_watch.map(|(d, _)| (d, ago(25)));
    a.health_tick(TICK, &tx);
    assert!(Arc::ptr_eq(a.live_worker.as_ref().unwrap(), &worker));
    // 31 s: hung. A new worker replaces it (here it fails to load its missing models, which is the next check's
    // business), and the island says so.
    a.worker_watch = a.worker_watch.map(|(d, _)| (d, ago(31)));
    a.health_tick(TICK, &tx);
    assert!(!Arc::ptr_eq(a.live_worker.as_ref().unwrap(), &worker), "the hung worker is still in use");
    assert!(a.index.evidence.records().unwrap().iter().any(|r| r.kind == "listen_worker_hung"));
    assert!(matches!(&health(&a)[..], [ui::HealthCue::Restarting { key, .. }] if key == "listen"));
    // Let the abandoned thread finish.
    drop(permit);
}

#[test]
fn listen_models_that_never_load_are_a_hang_after_two_minutes() {
    let (_t, mut a) = app();
    let (tx, _rx) = crossbeam_channel::bounded(512);
    a.listening_starting();
    let (release, wait) = mpsc::channel::<()>();
    let worker = Arc::new(listen::ListenWorker::spawn(Default::default(), move || -> Result<ListenEngine<Quiet, Held, Quiet>> {
        let _ = wait.recv();
        anyhow::bail!("released")
    }).unwrap());
    a.live_worker = Some(worker.clone());
    a.worker_spawned = Some(ago(60));
    a.health_tick(TICK, &tx);
    assert!(Arc::ptr_eq(a.live_worker.as_ref().unwrap(), &worker), "a minute of loading is normal");
    a.worker_spawned = Some(ago(121));
    a.health_tick(TICK, &tx);
    assert!(!Arc::ptr_eq(a.live_worker.as_ref().unwrap(), &worker));
    drop(release);
}

#[test]
fn the_microphone_switch_and_its_return_show_on_the_island_only_while_listening() {
    let (_t, mut a) = app();
    let dev = |uid: &str, name: &str, built_in| capture::InputDevice { uid: uid.into(), name: name.into(), built_in };
    let state = |devices: Vec<capture::InputDevice>, in_use: Option<&str>| capture::MicrophoneState {
        devices,
        chosen: Some("AirPods".into()),
        in_use: in_use.map(Into::into),
        extras: vec![],
        open: in_use.into_iter().map(Into::into).collect(),
    };
    a.settings.microphone = Some(crate::settings::MicrophoneChoice { uid: "AirPods".into(), name: "AirPods Pro".into() });
    // Off: capture still reports devices (discovery), but nothing is said.
    a.microphone_state(&state(vec![dev("BuiltIn", "MacBook Pro Microphone", true)], Some("BuiltIn")));
    assert!(health(&a).is_empty());
    a.listening_starting();
    a.microphone_state(&state(vec![dev("BuiltIn", "MacBook Pro Microphone", true), dev("AirPods", "AirPods Pro", false)], Some("AirPods")));
    a.microphone_state(&state(vec![dev("BuiltIn", "MacBook Pro Microphone", true)], Some("BuiltIn")));
    assert_eq!(health(&a), [ui::HealthCue::Notice { part: ui::Part::Microphone, label: "AirPods Pro → MacBook Pro Microphone".into() }]);
    // Lid closed in clamshell mode with the AirPods gone: no microphone at all.
    a.microphone_state(&state(vec![], None));
    assert!(matches!(health(&a).last(), Some(ui::HealthCue::Problem { key, control: ui::HealthControl::OpenMenu, .. }) if key == "mic"));
}

#[test]
fn a_decider_that_does_not_load_backs_off_without_blocking_the_pipeline() {
    let (_t,mut a)=app();
    for round in 0..2 {
        a.inference().unwrap().task(2,|state|{assert!(state.decider().is_err());Box::new(|_|Ok(()))}).unwrap();
        loop {let e=a.internal.1.recv_timeout(Duration::from_secs(2)).unwrap();a.internal(e).unwrap();if a.send_worker.as_ref().unwrap().outstanding.load(Ordering::Acquire)==0 {break;}}
        let loads=a.index.evidence.records().unwrap().iter().filter(|r|r.kind=="decider_load_failed").count();
        assert_eq!(loads,1,"round {round}: backoff must avoid another load");
    }
    assert!(health(&a).iter().any(|c|matches!(c,ui::HealthCue::Drop {key,..} if key=="decisions")));
    a.set_running(false,"test").unwrap();
    assert!(a.send_worker.is_none());
}

#[test]
fn the_disk_floor_is_red_until_space_returns_and_a_stronger_recovery_is_never_downgraded() {
    let (_t, mut a) = app();
    a.disk_health(Some("Archive stopped: 1.0 GiB free; 2.0 GiB floor"));
    a.disk_health(Some("Archive stopped: 1.0 GiB free; 2.0 GiB floor"));
    a.disk_health(None);
    let cues = health(&a);
    assert!(matches!(&cues[..], [
        ui::HealthCue::Problem { label, control: ui::HealthControl::ShowArchive, .. },
        ui::HealthCue::Recovered { label: ok, .. },
    ] if label == "Disk almost full · not recording" && ok == "Recording again"), "{cues:?}");
    // A damaged database says so in the user's words; the SQLite error stays in the log and the journal.
    a.disk_health(Some("Archive stopped: database disk image is malformed (code 11)"));
    assert!(matches!(health(&a).last(), Some(ui::HealthCue::Problem { label, action, control: ui::HealthControl::ShowArchive, .. })
        if label == "Archive database is damaged" && action == "Click to show it in Finder"));
    a.disk_health(Some("Archive stopped: Permission denied (os error 13)"));
    assert!(matches!(health(&a).last(), Some(ui::HealthCue::Problem { label, action, .. })
        if label == "Archive stopped · not recording" && action == "Click to show it in Finder"));
    a.request_recovery(Recovery::Relaunch { key: "listen".into(), part: ui::Part::Transcription, problem: "x".into() });
    a.request_recovery(Recovery::RestartCapture("y".into()));
    assert!(matches!(a.take_recovery(), Some(Recovery::Relaunch { .. })));
    assert_eq!(a.take_recovery(), None);
}

#[test]
fn a_damaged_archive_store_says_so_instead_of_restarting_into_the_same_file() {
    let (_t, mut a) = app();
    // The store's own SQLite error, as capture's archive pipeline reports it.
    a.capture_event(capture::CaptureEvent::Gap {
        source_id: None,
        host_ns: 1,
        detail: "archive pipeline stopped: archive index: database disk image is malformed (code 11)".into(),
    })
    .unwrap();
    assert!(matches!(health(&a).last(), Some(ui::HealthCue::Problem { label, control: ui::HealthControl::ShowArchive, .. })
        if label == "Archive database is damaged"), "{:?}", health(&a));
    assert_eq!(a.take_recovery(), None, "a restart would reopen the same damaged store");
    // Any other archive error still restarts capture.
    a.capture_event(capture::CaptureEvent::Gap { source_id: None, host_ns: 2, detail: "archive pipeline stopped: write failed".into() }).unwrap();
    assert!(matches!(a.take_recovery(), Some(Recovery::RestartCapture(_))));
}

fn fake_pi(dir:&std::path::Path,version:&str)->std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;let p=dir.join("pi");std::fs::write(&p,format!("#!/bin/sh\necho '{version}'\n")).unwrap();std::fs::set_permissions(&p,std::fs::Permissions::from_mode(0o700)).unwrap();p
}

fn settle(a: &mut Pipeline) {
    let rx = a.internal_events();
    let e = rx.recv_timeout(Duration::from_secs(15)).expect("pre-flight answer");
    a.internal(e).unwrap();
}

#[test]
fn wrong_runtime_version_keeps_agent_off_and_newer_off_wins() {
    use agent::delivery::StopReason;
    let (t,mut a)=app();crate::bridge::test_harness(Some(fake_pi(t.path(),"1.0.2")));
    a.tools.menu_intent(StopReason::Agent,true);a.action(ui::UiAction::SetDryRun(false)).unwrap();settle(&mut a);
    assert!(a.menu_agent_off());assert!(health(&a).iter().any(|c|matches!(c,ui::HealthCue::Problem{label,..} if label=="Agent runtime in Heimdall is damaged")));
    crate::bridge::test_harness(Some(fake_pi(t.path(),&agent::runtime::pi_version())));
    a.tools.menu_intent(StopReason::Agent,true);a.action(ui::UiAction::SetDryRun(false)).unwrap();a.tools.menu_intent(StopReason::Agent,false);a.action(ui::UiAction::SetDryRun(true)).unwrap();settle(&mut a);
    assert!(a.menu_agent_off());crate::bridge::test_harness(None);a.finish().unwrap();
}

#[test]
fn screen_recreation_clears_health_and_status_and_records_the_cause() {
    let (_t, mut a) = app();
    a.screen_event(screen::ScreenEvent::Error("SCK display 1 stopped: test-domain code 7".into()));
    a.screen_event(screen::ScreenEvent::Recovering { attempt: 1, delay_seconds: 1 });
    a.screen_event(screen::ScreenEvent::DisplaysChanged { displays: 1 });
    assert!(!a.health.holds("screen"));
    assert!(!a.component_status.contains_key("screen"));
    assert!(matches!(health(&a).last(), Some(ui::HealthCue::Recovered { key, .. }) if key == "screen"));
    a.evidence.flush().unwrap();
    let records = a.index.evidence.records().unwrap();
    assert!(records.iter().any(|r| r.kind == "health" && r.payload["reason"].as_str().is_some_and(|s| s.contains("test-domain code 7"))));
}

#[test]
fn screen_zero_displays_and_configuration_ack_cannot_claim_recovery() {
    let (_t, mut a) = app();
    a.screen_event(screen::ScreenEvent::Started { displays: 0 });
    assert!(a.health.holds("screen") && a.component_status.contains_key("screen"));
    a.screen_event(screen::ScreenEvent::Configured { error: None });
    assert!(a.health.holds("screen") && a.component_status.contains_key("screen"));
    a.screen_event(screen::ScreenEvent::Paused { reason: "session locked".into() });
    a.evidence.flush().unwrap();
    let before = a.index.evidence.records().unwrap().len();
    for _ in 0..100 { a.screen_event(screen::ScreenEvent::Paused { reason: "session locked".into() }); }
    a.evidence.flush().unwrap();
    assert_eq!(a.index.evidence.records().unwrap().len(), before, "no per-tick pause records");
    a.screen_event(screen::ScreenEvent::DisplaysChanged { displays: 1 });
    assert!(!a.health.holds("screen") && !a.component_status.contains_key("screen"));
    a.screen_event(screen::ScreenEvent::Unavailable);
    a.screen_stopped("Seeing off");
    assert!(!a.health.holds("screen") && !a.component_status.contains_key("screen"));
}

#[test]
fn component_failures_settings_and_recovery_are_durable_transitions() {
    let (_t, mut a) = app();
    a.component_status("embedding-index", Some("Embedding index: model request timed out".into()));
    a.evidence.flush().unwrap();
    let before = a.index.evidence.records().unwrap().len();
    for _ in 0..100 { a.component_status("embedding-index", Some("Embedding index: model request timed out".into())); }
    a.evidence.flush().unwrap();
    assert_eq!(a.index.evidence.records().unwrap().len(), before);
    a.component_status("embedding-index", None);
    a.component_status_reason("archive", Some("Archive paused".into()), Some("2.0 GiB floor"));
    a.component_status_reason("archive", Some("Archive paused".into()), Some("Permission denied"));
    // A worker that could not start is held ("Listen: …"); a one-off "Listen failed: …" is only noted (finding 11).
    a.fail("Listen: test model load error");
    a.fail("Listen: test model load error"); // Same held failure: no duplicate.
    a.component_status("listen", None);
    a.fail("Listen: test model load error"); // Same cause after recovery: a new outage.
    a.settings_result("Screen Recording", Some("SCK rejected width".into()));
    a.settings_result("Screen Recording", None);
    a.evidence.flush().unwrap();
    let rows = a.index.evidence.records().unwrap();
    assert_eq!(rows.iter().filter(|r| r.payload["reason"] == "Listen: test model load error").count(), 2);
    assert!(rows.iter().any(|r| r.payload["reason"] == "SCK rejected width"));
    assert!(rows.iter().any(|r| r.payload["reason"] == "Permission denied"));
    assert!(rows.iter().any(|r| r.payload["component"] == "embedding-index" && r.payload["state"] == "recovered"));
}

#[test]
fn watchdog_diagnostic_uses_the_current_archive_without_waiting_for_its_mutex() {
    let (_t, a) = app();
    a.tools.record_diagnostic("health", serde_json::json!({"component":"watchdog", "reason":"the menu stopped responding"}));
    assert!(a.index.evidence.records().unwrap().iter().any(|r| r.payload["reason"] == "the menu stopped responding"));
    let _held = a.tools.archive_session.lock().unwrap();
    // No deadlock if this is the very mutex the frozen thread holds.
    a.tools.record_diagnostic("health", serde_json::json!({"component":"watchdog", "reason":"locked"}));
}

#[test]
fn a_locked_archive_cannot_block_watchdog_diagnostics() {
    let (_t, a) = app();
    let root = a.session.parent().unwrap();
    let db = rusqlite::Connection::open(root.join(heimdall_archive::store::INDEX)).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let start = Instant::now();
    a.tools.record_diagnostic("health", serde_json::json!({"component":"watchdog", "reason":"held database transaction"}));
    assert!(start.elapsed() < Duration::from_secs(1), "watchdog waited for SQLite's writer timeout");
    db.execute_batch("ROLLBACK").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if a.index.evidence.records().unwrap().iter().any(|r| r.payload["reason"] == "held database transaction") { break; }
        assert!(Instant::now() < deadline, "independent diagnostic did not commit after unlock");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bug note 2026-11-02T15-58-15: the screen gave up during a 110 s archive stall and stayed off after the archive
/// caught up, waiting only for a wake or a display change. Catching up restarts it; a healthy stream is left alone.
#[test]
fn the_archive_catching_up_restarts_a_screen_that_gave_up() {
    let (_t, mut a) = app();
    a.screen_event(screen::ScreenEvent::DisplaysChanged { displays: 1 });
    a.archive_caught_up();
    assert!(a.take_recovery().is_none(), "a running screen is not restarted");
    a.screen_event(screen::ScreenEvent::Unavailable);
    a.archive_caught_up();
    assert!(matches!(a.take_recovery(), Some(crate::health::Recovery::RestartScreen(why)) if why == "the archive caught up"));
    a.screen_event(screen::ScreenEvent::DisplaysChanged { displays: 1 });
    a.archive_caught_up();
    assert!(a.take_recovery().is_none(), "recovered: nothing more to restart");
    a.screen_event(screen::ScreenEvent::Unavailable);
    a.screen_stopped("Seeing off");
    a.archive_caught_up();
    assert!(a.take_recovery().is_none(), "Seeing off: nothing to restart");
}
