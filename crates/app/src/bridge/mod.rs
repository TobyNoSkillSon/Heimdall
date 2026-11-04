use crate::{
    index::ArchiveSearch,
    settings::Settings,
};
use agent::{
    harness::{Harness, Pi, PiConfig},
    protocol::{Attachment, Event, Observation},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, oneshot};


mod adapters;
mod hold;
mod speech;
/// When Heimdall makes a sound and when it keeps quiet: the wake sound, conversations (night pass 3).
pub(crate) mod speech_policy;
pub(crate) mod echo_cancel;
mod intent;
mod tools;
mod voice_preview;
/// Replies shown as text that the user has probably not read yet (bug note 22 Oct 15:13).
mod unread;
mod supervisor;
mod delivery;
mod typed_arrival;
mod health;
mod startup;
mod compaction_hold;
mod compaction_fallback;
mod window;
/// Voice answers to `ask` cards (night-ask).
pub(crate) mod questions;
pub(crate) mod ask_voice;
use self::{adapters::*, speech::*, intent::*, delivery::*, health::*, startup::*, window::*, compaction_hold::*, compaction_fallback::CompactionFallback};
pub(crate) use self::health::LabAgentHealth;
pub(crate) use self::speech::LazySynth;
pub(crate) use self::speech::SampleVoice;
pub(crate) use self::speech::attach_speech_level;
pub use self::speech::VOICE_SAMPLE;
pub use self::tools::Tools;
pub use self::intent::IntentGeneration;
pub use self::tools::SCREEN_OFF;
pub(crate) use self::tools::archive_screenshot_action;
pub(crate) use self::tools::{context_attachment, CONTEXT_EDGE_PX};
pub use self::supervisor::AgentRuntime;
pub(crate) use self::delivery::window_problem;
pub(crate) use self::delivery::connection_notice_ids;
pub(crate) use self::delivery::notice_outcome;
pub(crate) use self::delivery::notice_outcomes;
pub(crate) use self::startup::agent_thinking;

pub enum Command {
    Scoped { command: Box<Command>, permit: agent::delivery::DeliveryPermit },
    NoticeAction { action: ui::agent_notice::AgentNoticeAction, permit: agent::delivery::DeliveryPermit },
    Observe(Observation),
    Wake {
        images: Vec<Attachment>,
        urgent: bool,
    },
    /// `speaker`: whose line the decision sent (`agent::Speaker`: the user's microphone or another source).
    WakePending { images:Vec<Attachment>, urgent:bool, through:u64, observation:Option<String>, speaker:agent::Speaker },
    /// A screenshot that arrived after its words went out: a steer into the running turn, or held for the next wake.
    LateImages { caption:String, images:Vec<Attachment> },
    Notice { id:String, text:String, permit:agent::delivery::DeliveryPermit, ack:std::sync::mpsc::Sender<std::result::Result<(),String>> },
    Restart(Settings),
    /// The thinking selector moved for the model the agent runs: the running Pi takes the new level from its next
    /// request (no restart); the settings are kept for later starts.
    Thinking(Settings),
    /// A provider's key was connected, replaced or forgotten: an agent on that provider restarts with it.
    ProviderKeyChanged(String),
    Send { text:String, steer:bool },
    /// The user's typed message whose bubble already shows (`Tools::window_action`, bug note 2026-11-01T00-20-27): sent
    /// as `Send`, under the delivery ID `echo` the bubble carries, so the delivered row replaces it in place.
    Typed { text:String, steer:bool, echo:String },
    /// A drop onto the notch on its own (no words of his took it): the user's typed message with its images
    /// (pipeline/drops.rs).
    Dropped { text:String, images:Vec<Attachment> },
    EditAndResend { session_entry_id:String, text:String },
    Reload { reason:String, abort:bool, permit:Option<agent::delivery::DeliveryPermit> },
    ResolveDelivery { id:String, completed:bool },
    Cancel,
    Barrier(oneshot::Sender<()>),
    /// Emergency mode ends (docs/emergency.md): the hold is released and the agent's next turn starts with `back`.
    EmergencyRelease { back: Option<String> },
    /// A relay run started or every one ended (`crate::emergency::RELAY_WORKING`): the agent's state is shown again.
    RelayState,
    #[cfg(test)]
    Inspect(oneshot::Sender<Option<(u32,PathBuf)>>),
    /// The settings the supervisor would start the next Pi with.
    #[cfg(test)]
    InspectSettings(oneshot::Sender<Settings>),
    Shutdown,
}
#[cfg(test)]
thread_local! { static TEST_HARNESS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) fn test_harness(path: Option<PathBuf>) { TEST_HARNESS.with(|p| *p.borrow_mut() = path); }
pub(crate) fn fixture_executable()->Option<PathBuf>{
    #[cfg(test)] {return TEST_HARNESS.with(|p|p.borrow().clone()).or_else(|| {
        // Explicit real-Pi labs (never set in ordinary test runs) exercise the production start path.
        if std::env::var_os("HEIMDALL_PI_SMOKE_DATA").is_some() || std::env::var_os("HEIMDALL_OPENCODE_LAB").is_some() {None}
        else {Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py"))}
    });}
    #[cfg(not(test))] {None}
}
fn spool_has_deliveries(p: &Path) -> Result<bool> {
    let c = rusqlite::Connection::open(p)?;
    Ok(c.query_row(
        "SELECT count(*) FROM deliveries WHERE state!='queued'",
        [],
        |r| r.get::<_, i64>(0),
    )? > 0)
}
pub fn clock_label() -> String {
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = libc::time(std::ptr::null_mut());
        libc::localtime_r(&t, &mut tm);
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    }
}

fn settle_dry(b: &mut agent::Bridge<RecordedAdapter>) -> Result<()> {
    loop {
        let ev = if let Adapter::Dry(h) = &mut b.harness.inner {
            h.rx.try_recv().ok()
        } else {
            None
        };
        match ev {
            Some(e) => {
                b.harness.record_event(&e)?;
                b.handle(&e)?;
            }
            None => break,
        }
    }
    Ok(())
}

// Wire images retain exact bytes via the same CAS object as the canonical envelope.
fn reference_transport_images(journal: &heimdall_archive::Journal, v: &mut Value) -> Result<()> {
    if v["type"] == "image" && v["data"].is_string() {
        use base64::Engine;
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap())?;
        let mime = v["mimeType"].as_str().context("wire image MIME")?;
        let id = journal.image(&bytes, mime)?;
        v.as_object_mut().unwrap().remove("data");
        v["archive_image_id"] = json!(id);
    }
    if v["type"] == "image" && v["source"]["data"].is_string() {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(v["source"]["data"].as_str().unwrap())?;
        let mime = v["source"]["media_type"]
            .as_str()
            .context("wire source MIME")?;
        let id = journal.image(&bytes, mime)?;
        v["source"].as_object_mut().unwrap().remove("data");
        v["source"]["archive_image_id"] = json!(id);
    }
    match v {
        Value::Array(a) => {
            for v in a {
                reference_transport_images(journal, v)?;
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                reference_transport_images(journal, v)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn collect_transport_ids(v: &Value, ownership: &HashMap<String, Vec<String>>, ids: &mut Vec<String>) {
    // RPC fields are not model content. Echoed user messages retain the delivery
    // marker for settlement; resolve it against the same map as agent_reply.
    ids.extend(agent::protocol::observation_ids(v));
    match v {
        Value::String(text) => {
            if let Some(id) = text.strip_prefix("[Heimdall delivery: ").and_then(|s|s.split_once("]\n").map(|(id,_)|id)) {
                if let Some(observations) = ownership.get(id) { ids.extend(observations.iter().cloned()); }
            }
        }
        Value::Array(a) => for v in a { collect_transport_ids(v, ownership, ids); },
        Value::Object(o) => for v in o.values() { collect_transport_ids(v, ownership, ids); },
        _ => {}
    }
}

#[cfg(test)]
#[path = "tests/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tests/recovery_tests.rs"]
mod recovery_tests;

#[cfg(test)]
#[path = "tests/off_regressions.rs"]
mod off_regressions;

#[cfg(test)]
#[path = "tests/stop_persistence_tests.rs"]
mod stop_persistence_tests;

#[cfg(test)]
#[path = "tests/pi_supervisor_tests.rs"]
mod pi_supervisor_tests;

#[cfg(test)]
#[path = "tests/stop_wait_tests.rs"]
mod stop_wait_tests;

#[cfg(test)]
#[path = "tests/live_pi_smoke.rs"]
mod live_pi_smoke;

/// Explicit real-Pi labs for the production start path (no fixture): the pinned runtime, the production extension
/// and MCP server, an API model selection, and a local fake of OpenCode Go reached through a lab Console sign-in
/// (kept in memory; no Keychain, installed app, real profile or cloud request). The runtime is installed once
/// under the lab path and linked into each case's data directory.
///
/// Run: HEIMDALL_OPENCODE_LAB=<abs path containing /target/opencode-lab> cargo test -p app --bin heimdall -- \
///      --ignored --test-threads=1 opencode_start_lab
#[cfg(test)]
#[path = "tests/opencode_start_lab.rs"]
mod opencode_start_lab;

#[cfg(test)]
#[path = "tests/ownership_notice_tests.rs"]
mod ownership_notice_tests;

#[cfg(test)]
#[path = "tests/release_blocker_regressions.rs"]
mod release_blocker_regressions;

#[cfg(test)]
#[path = "tests/jarvis_voice_tests.rs"]
mod jarvis_voice_tests;
#[cfg(test)]
#[path = "tests/diagnostic_tests.rs"]
mod diagnostic_tests;

#[cfg(test)]
#[path = "tests/delivery_regressions.rs"]
mod delivery_regressions;

#[cfg(test)]
#[path = "tests/image_tests.rs"]
mod image_tests;

#[cfg(test)]
#[path = "tests/relay_activity_tests.rs"]
mod relay_activity_tests;

#[cfg(test)]
#[path = "tests/voice_tests.rs"]
mod voice_tests;

// Scheduled wakes (agent::wakes, selfwake night 21 Oct) through the real supervisor and outbox.
#[cfg(test)]
#[path = "tests/selfwake_tests.rs"]
mod selfwake_tests;
