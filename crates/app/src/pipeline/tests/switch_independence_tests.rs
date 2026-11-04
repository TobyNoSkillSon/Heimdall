//! The five menu switches are independent: each turns off only itself. Listening off stops
//! capture and transcription, never the agent, speech or the screen; Agent off stops only the reasoning agent, never
//! listening or the decider; and the agent is never told that Listening or Seeing is off. Through the real `Pipeline`
//! with stubbed models and the fake Pi (`plumbing_tests::rig`).
use super::plumbing_tests::{rig, turn, Detect};
use super::*;
use agent::delivery::StopReason;
use std::sync::atomic::AtomicUsize;

const S: u64 = 1_000_000_000;

fn records(a: &Pipeline) -> Vec<heimdall_archive::Record> {
    a.index.evidence.records().unwrap()
}
/// Every wire Heimdall queued for or delivered to Pi (delivered ones are retired to the session archive).
fn wires(a: &Pipeline) -> Vec<String> {
    let Ok(db) = rusqlite::Connection::open(a.data.join("pi-spool.sqlite")) else { return vec![] };
    let Ok(mut q) = db.prepare("SELECT wire FROM deliveries") else { return vec![] };
    let rows: Vec<String> = q.query_map([], |r| r.get(0)).unwrap().map(Result::unwrap).collect();
    rows.into_iter().map(|wire| {
        let value: serde_json::Value = serde_json::from_str(&wire).unwrap();
        match value["archive_ref"].as_str() {
            Some(r) => String::from_utf8(heimdall_archive::store::read_log(&a.session, r).unwrap()).unwrap(),
            None => wire,
        }
    }).collect()
}
fn wait_for_wire(a: &Pipeline, what: &str, text: &str) {
    let began = std::time::Instant::now();
    while !wires(a).iter().any(|w| w.contains(text)) {
        assert!(began.elapsed() < std::time::Duration::from_secs(10), "{what} never reached the agent");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
/// Words that would tell the agent a switch is off. None of them may reach it.
fn tells_a_switch(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    ["listening", "seeing", "paused", "heimdall off", "heimdall is off", "switched off", "screen is off", "microphone is off", "mic is off"]
        .into_iter().find(|w| lower.contains(w))
}
/// Listening off exactly as the live loop processes it after capture stopped: the latch from the callback, the
/// processed intent, then the sources close (`live_loop::reconcile_listening`).
fn listening_off(a: &mut Pipeline) {
    a.tools.menu_intent(StopReason::Heimdall, false);
    a.heimdall_intent_processed().unwrap();
    let (_tx, rx) = crossbeam_channel::unbounded();
    a.stop_listening(&rx).unwrap();
}
struct Synth(Arc<AtomicUsize>);
impl speak::Synthesizer for Synth {
    fn synthesize(&mut self, _: &str, _: &str, _: &dyn Fn() -> bool) -> Result<speak::Audio> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(speak::Audio { samples: vec![0.; 160], sample_rate: 16000, first_audio_ms: 0., synthesis_ms: 1. })
    }
}
struct Output;
impl speak::AudioOutput for Output {
    fn output(&mut self, _: &speak::Audio, _: &dyn Fn() -> bool) -> Result<()> { Ok(()) }
}

/// Before 22 Oct, Listening off took the agent down (`set_running` dropped the runtime and speech, the shared stop
/// latch closed delivery). Now the agent keeps running: a typed message, a worker notice and a scheduled wake all
/// reach Pi, it still speaks with Speaking on, the screen buffer stays, and nothing it is sent says Listening is off.
#[test]
fn listening_off_leaves_the_agent_running_and_delivering() {
    let (_t, mut a, mic) = rig(true);
    turn(&mut a, &mic, 2 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let permit = a.tools.agent_enabled.permit().unwrap();
    *a.tools.buffer.lock().unwrap() = Some(a.screen_buffer.clone());
    let frames = a.screen_buffer.lock().unwrap().frame_count();
    assert!(frames > 0);

    listening_off(&mut a);
    assert!(a.settings.paused && a.tools.stopped_for(StopReason::Heimdall), "Listening is off");
    assert!(a.sources.is_empty(), "the sources closed");
    assert!(a.agent.as_ref().is_some_and(|r| r.is_running()), "Listening off stopped the agent");
    assert!(a.tools.agent_enabled.load(Ordering::Acquire) && !a.menu_agent_off(), "Listening off closed delivery");
    assert!(permit.check().is_ok(), "Listening off revoked the agent's epoch");
    assert!(a.tools.enabled.load(Ordering::Acquire), "Listening off silenced Speaking");
    assert!(a.tools.buffer.lock().unwrap().is_some(), "Listening off stopped Seeing");
    assert_eq!(a.screen_buffer.lock().unwrap().frame_count(), frames, "Listening off dropped screen frames");

    // A typed message (Debug Agent composer), a worker notice (T3) and a scheduled wake, all after Off.
    a.tools.window_action(ui::AgentWindowAction::Send { text: "typed while the mic is closed".into(), steer: false }).unwrap();
    wait_for_wire(&a, "the typed message", "typed while the mic is closed");
    a.tools.notice("terminal:t:r:completed", "Worker t run r completed.", &a.tools.agent_enabled.permit().unwrap()).unwrap();
    wait_for_wire(&a, "the worker notice", "Worker t run r completed.");
    let wakes = a.tools.mcp_actions.wakes.as_ref().map_err(|e| anyhow::anyhow!(e.clone())).unwrap().clone();
    wakes.schedule("op-off", &agent::wakes::When::In { ms: 200 }, "check the nightly build", agent::wakes::Source::Agent).unwrap();
    wait_for_wire(&a, "the scheduled wake", "check the nightly build");

    // Speaking on: the agent still talks.
    let spoken = Arc::new(AtomicUsize::new(0));
    a.tools.test_guarded_speech_queue(Synth(spoken.clone()), Output);
    let said = a.tools.mcp_actions.speak(&a.tools, "The build is green.", "en", "speak-off", &a.tools.agent_enabled.permit().unwrap()).unwrap();
    assert!(said.starts_with("Speech queued"), "{said}");
    let began = std::time::Instant::now();
    while spoken.load(Ordering::SeqCst) == 0 {
        assert!(began.elapsed() < std::time::Duration::from_secs(10), "the reply was never synthesized");
        a.tools.speech_status();
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // Nothing the agent was given tells it a switch is off.
    for wire in wires(&a) {
        assert!(tells_a_switch(&wire).is_none(), "the agent was told {:?}: {wire}", tells_a_switch(&wire));
    }
    a.tools.shutdown();
}

/// Listening off is stop-persisted on its own: a relaunch starts with Listening off and the agent's delivery open.
#[test]
fn listening_off_persists_without_stopping_the_agent_after_relaunch() {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    let mut a = Pipeline::new(data.clone(), t.path().into(), Settings { dry_run: false, ..Default::default() }, None, true).unwrap();
    a.test_persist_settings();
    a.save_settings().unwrap();
    let before = a.tools.agent_enabled.generation();
    a.tools.menu_intent(StopReason::Heimdall, false);
    assert_eq!(a.tools.agent_enabled.generation(), before, "Listening off moved the agent's delivery epoch");
    a.heimdall_intent_processed().unwrap();
    let saved = Settings::load(&data.join("settings.json")).unwrap();
    assert!(saved.paused && !saved.dry_run);
    let tools = crate::bridge::Tools::new(&data, &data);
    tools.initialize_delivery(&saved);
    assert!(tools.stopped_for(StopReason::Heimdall), "Listening off did not survive the relaunch");
    assert!(tools.agent_enabled.permit().is_ok(), "a relaunch with Listening off started with the agent stopped");
    a.tools.shutdown();
}

/// Agent off stops only the reasoning agent: the sources stay, finals are still transcribed and decided by d1, and
/// nothing reaches Pi.
#[test]
fn agent_off_leaves_listening_and_the_decider_running() {
    let (_t, mut a, mic) = rig(true);
    turn(&mut a, &mic, 2 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let sent_before = records(&a).iter().filter(|r| r.kind == "agent_transport_sent").count();
    let decided_before = records(&a).iter().filter(|r| r.kind == "send_decision").count();
    a.tools.menu_intent(StopReason::Agent, false);
    a.action(ui::UiAction::SetDryRun(true)).unwrap();
    assert!(a.settings.dry_run && a.menu_agent_off() && !a.tools.agent_enabled.load(Ordering::Acquire));
    assert!(!a.settings.paused && !a.tools.stopped_for(StopReason::Heimdall), "Agent off turned Listening off");
    assert!(!a.sources.is_empty(), "Agent off closed the sources");

    turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.99), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let rows = records(&a);
    assert!(rows.iter().filter(|r| r.kind == "send_decision").count() > decided_before, "the decider stopped with Agent off");
    assert!(a.send_worker.is_some(), "Agent off unloaded the decider");
    assert!(a.index.utterances_between(10 * S, 20 * S).unwrap().iter().any(|u| u.to_string().contains("check the build")), "the final was not kept");
    assert_eq!(rows.iter().filter(|r| r.kind == "agent_transport_sent").count(), sent_before, "something reached Pi with Agent off");
    a.tools.shutdown();
}

/// What the agent can read about the screen and its tools never names Listening or Seeing: a screenshot with Seeing
/// off fails plainly, and no tool description, prompt line or transcript note says a switch is off.
#[test]
fn no_agent_visible_text_names_listening_or_seeing() {
    let mut texts: Vec<(String, String)> = vec![
        ("SCREEN_OFF".into(), crate::bridge::SCREEN_OFF.into()),
        ("SCREEN_OFF_NOTE".into(), super::commands::SCREEN_OFF_NOTE.into()),
        ("screen_frames".into(), crate::mcp_frames::SCREEN_FRAMES_UNAVAILABLE.into()),
        ("prompt".into(), include_str!("../../../../agent/prompt/heimdall.md").into()),
        ("compaction prompt".into(), include_str!("../../../../agent/prompt/compaction.md").into()),
    ];
    for tool in agent::mcp_contract::tools().as_array().unwrap() {
        texts.push((tool["name"].as_str().unwrap().into(), tool["description"].as_str().unwrap_or_default().into()));
    }
    for (name, text) in &texts {
        assert!(tells_a_switch(text).is_none(), "{name} tells the agent {:?}: {text}", tells_a_switch(text));
    }
    // The tool itself, with Seeing off and the agent on.
    let t = tempfile::tempdir().unwrap();
    let tools = crate::bridge::Tools::new(t.path(), t.path());
    tools.screen_off.store(true, Ordering::Release);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    use agent::mcp::ToolHost;
    let shot = rt.block_on(tools.screenshot_now(&tools.agent_enabled.permit().unwrap())).unwrap();
    assert!(shot.images.is_empty() && !shot.notes.is_empty(), "{:?}", shot.notes);
    assert!(shot.notes.iter().all(|n| tells_a_switch(n).is_none()), "{:?}", shot.notes);
}

/// [n23-menu] Seeing off stops only the screen: the sources stay open, finals are still transcribed, decided and
/// delivered, a typed message and a worker notice still reach Pi, Listening and Agent stay on, and nothing the agent
/// is sent says Seeing is off.
#[test]
fn seeing_off_leaves_listening_the_decider_and_the_agent_running() {
    let (_t, mut a, mic) = rig(true);
    turn(&mut a, &mic, 2 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    let permit = a.tools.agent_enabled.permit().unwrap();
    let decided_before = records(&a).iter().filter(|r| r.kind == "send_decision").count();

    a.set_seeing(false).unwrap();
    assert!(a.settings.screen_off && a.tools.screen_off.load(Ordering::Acquire), "Seeing is off");
    assert!(!a.settings.paused && !a.tools.stopped_for(StopReason::Heimdall), "Seeing off turned Listening off");
    assert!(!a.sources.is_empty(), "Seeing off closed the sources");
    assert!(a.agent.as_ref().is_some_and(|r| r.is_running()), "Seeing off stopped the agent");
    assert!(a.tools.agent_enabled.load(Ordering::Acquire) && !a.menu_agent_off(), "Seeing off closed delivery");
    assert!(permit.check().is_ok(), "Seeing off revoked the agent's epoch");
    assert!(a.tools.enabled.load(Ordering::Acquire), "Seeing off silenced Speaking");

    turn(&mut a, &mic, 10 * S, "Heimdall check the build", Detect::Final, 0, Some(0.99), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    assert!(records(&a).iter().filter(|r| r.kind == "send_decision").count() > decided_before, "the decider stopped with Seeing off");
    assert!(a.index.utterances_between(10 * S, 20 * S).unwrap().iter().any(|u| u.to_string().contains("check the build")), "the final was not kept");
    wait_for_wire(&a, "the spoken wake", "check the build");
    a.tools.window_action(ui::AgentWindowAction::Send { text: "typed with the screen off".into(), steer: false }).unwrap();
    wait_for_wire(&a, "the typed message", "typed with the screen off");
    a.tools.notice("terminal:t:r2:completed", "Worker t run r2 completed.", &a.tools.agent_enabled.permit().unwrap()).unwrap();
    wait_for_wire(&a, "the worker notice", "Worker t run r2 completed.");

    for wire in wires(&a) {
        assert!(tells_a_switch(&wire).is_none(), "the agent was told {:?}: {wire}", tells_a_switch(&wire));
    }
    a.tools.shutdown();
}

/// [n23-menu] Listening off and back on is silent to the agent: the next typed message arrives with no note that the
/// microphone was closed or reopened.
#[test]
fn listening_off_and_on_again_tells_the_agent_nothing() {
    let (_t, mut a, mic) = rig(true);
    turn(&mut a, &mic, 2 * S, "I was thinking about the build", Detect::Final, 0, Some(0.), 0, |_| {});
    a.agent.as_ref().unwrap().barrier().unwrap();
    listening_off(&mut a);
    a.tools.menu_intent(StopReason::Heimdall, true);
    a.heimdall_intent_processed().unwrap();
    assert!(!a.settings.paused && !a.tools.stopped_for(StopReason::Heimdall), "Listening is back on");
    a.tools.window_action(ui::AgentWindowAction::Send { text: "typed after the mic came back".into(), steer: false }).unwrap();
    wait_for_wire(&a, "the typed message", "typed after the mic came back");
    for wire in wires(&a) {
        assert!(tells_a_switch(&wire).is_none(), "the agent was told {:?}: {wire}", tells_a_switch(&wire));
    }
    a.tools.shutdown();
}

/// Show Islands and ⌃⌘,: one switch for every floating island, remembered across
/// restarts, independent of the others.
#[test]
fn show_islands_switches_every_island_and_persists() {
    let t = tempfile::tempdir().unwrap();
    let data = t.path().join("data");
    let mut a = Pipeline::new(data.clone(), t.path().into(), Settings { dry_run: false, ..Default::default() }, None, true).unwrap();
    a.test_persist_settings();
    a.save_settings().unwrap();
    assert!(a.ui_settings_value().islands_shown, "shown by default");
    let before = a.settings.clone();
    a.action(ui::UiAction::SetIslandsShown(false)).unwrap();
    assert!(a.settings.islands_hidden && !a.ui_settings_value().islands_shown && crate::islands::visibility::hidden());
    assert!((a.settings.paused, a.settings.dry_run, a.settings.screen_off) == (before.paused, before.dry_run, before.screen_off), "only itself");
    a.save_settings().unwrap();
    assert!(Settings::load(&data.join("settings.json")).unwrap().islands_hidden, "remembered across a restart");
    a.action(ui::UiAction::SetIslandsShown(true)).unwrap();
    assert!(!a.settings.islands_hidden && a.ui_settings_value().islands_shown && !crate::islands::visibility::hidden());
    a.save_settings().unwrap();
    let raw = std::fs::read_to_string(data.join("settings.json")).unwrap();
    assert!(!raw.contains("islands_hidden"), "shown is the default and is not written");
    let old: Settings = serde_json::from_str(r#"{"speak_replies":true}"#).unwrap();
    assert!(!old.islands_hidden, "a file from before the switch shows the islands");
    a.tools.shutdown();
}
