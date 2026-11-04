use speakers::{
    call::{CallScopedSpeakers, QUIET_TIMEOUT},
    identity::EnrollmentStore,
    naming::NamingFlow,
};
use std::{path::PathBuf, time::Instant};
#[test]
fn standby_never_loads_models() {
    let now = Instant::now();
    let mut call = CallScopedSpeakers::new(
        PathBuf::from("missing-helper"),
        PathBuf::from("missing-models"),
        0,
        NamingFlow::new(EnrollmentStore::default()),
    );
    assert!(!call.is_active());
    assert!(call.worker_pid().is_none());
    assert!(call.expire(now + QUIET_TIMEOUT).unwrap().is_empty());
    assert!(call.activate("call".into(), 1, 0., now).is_err());
    assert!(!call.is_active());
    assert!(call.worker_pid().is_none());
    assert!(call.stop().unwrap().is_empty());
    assert!(call.naming_mut().store.names.is_empty());
}
#[test]
fn call_worker_teardown_and_reactivation() {
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    if !models.join("Nemotron-3-Diarization.q8_0.gguf").is_file() {
        eprintln!("skipped: fetch pinned speaker assets first");
        return;
    }
    let executable = PathBuf::from(env!("CARGO_BIN_EXE_heimdall-speakers-worker"));
    let mut call = CallScopedSpeakers::new(
        executable,
        models,
        0,
        NamingFlow::new(EnrollmentStore::default()),
    );
    let now = Instant::now();
    call.activate("call".into(), 1, 0., now).unwrap();
    let pid = call.worker_pid().unwrap();
    call.push(&vec![0.; 16000], true, now).unwrap();
    // 20 Oct: in-call pauses of 18–33 s each ended the session, and with it the
    // speakers' names. Such a pause keeps the same session and helper.
    for pause in [18, 33, 60] {
        assert!(call.expire(now + std::time::Duration::from_secs(pause)).unwrap().is_empty());
        assert_eq!(call.worker_pid(), Some(pid), "a {pause} s pause keeps the session");
    }
    assert!(call
        .expire(now + QUIET_TIMEOUT - std::time::Duration::from_millis(1))
        .unwrap()
        .is_empty());
    assert!(call.is_active());
    call.expire(now + QUIET_TIMEOUT).unwrap();
    assert!(!call.is_active());
    assert!(call.worker_pid().is_none());
    assert_eq!(
        unsafe { libc::kill(pid as i32, 0) },
        -1,
        "child must be reaped"
    );
    call.activate("call".into(), 2, 1., now + QUIET_TIMEOUT)
        .unwrap();
    let pid = call.worker_pid().unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    assert!(call.stop().is_err());
    assert!(!call.is_active());
    assert!(call.worker_pid().is_none());
}

#[test]
fn call_worker_uses_selected_strategy_and_geometry() {
    use speakers::{live::LiveStrategy, pipeline::Diarization};
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../models/speakers");
    if !models.join("Sortformer_v2.1.mlmodelc").is_dir() {
        eprintln!("skipped: pinned Sortformer asset missing");
        return;
    }
    let mut call = CallScopedSpeakers::new(
        PathBuf::from(env!("CARGO_BIN_EXE_heimdall-speakers-worker")),
        models,
        0,
        NamingFlow::new(EnrollmentStore::default()),
    );
    call.set_strategy(LiveStrategy::Sortformer).unwrap();
    let now = Instant::now();
    call.activate("call".into(), 1, 0., now).unwrap();
    assert_eq!(call.strategy(), LiveStrategy::Sortformer);
    let stream = call.pipeline_mut().unwrap().stream_ref();
    assert_eq!(stream.speakers(), 4);
    assert_eq!(stream.seconds_per_frame(), 0.08);
    assert!(call.set_strategy(LiveStrategy::Nemotron).is_err());
    call.push(&vec![0.; 16000], false, now).unwrap();
    let pid = call.worker_pid().unwrap();
    call.expire(now + QUIET_TIMEOUT).unwrap();
    assert!(!call.is_active());
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    call.set_strategy(LiveStrategy::Nemotron).unwrap();
}
