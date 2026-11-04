//! Sparse, bounded evidence plus an independent rotating stdout/stderr sink.
use heimdall_archive::{Record, records::Writer};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Mutex, OnceLock},
};
#[path = "diagnostics_host.rs"]
mod host;
#[path = "diagnostics_log.rs"]
mod log;
/// The host's available memory estimate now (free + inactive + purgeable), for a background job deciding whether to
/// load something.
pub(crate) fn available_memory_bytes() -> Option<u64> {
    host::sample()["memory"]["available_estimate_bytes"].as_u64()
}
#[path = "diagnostics_lines.rs"]
mod lines;
pub(crate) use lines::line;
/// Tests: the log line a record gets.
#[cfg(test)]
pub(crate) use lines::evidence as evidence_line;

/// The open-file limit Heimdall asks for at startup. A launchd app starts with a soft limit of 256: 26 Oct 01:15 and
/// 01:36 the table filled and Jarvis, the screen and the spend log failed with "Too many open files (os error 24)".
/// 10240 is macOS's per-process `OPEN_MAX`.
const OPEN_FILES_WANTED: u64 = 10_240;
static OPEN_FILES: OnceLock<(u64, u64, Option<String>)> = OnceLock::new();
/// Raises the soft open-file limit toward `OPEN_FILES_WANTED` (never past the hard limit), once, first thing in `main`;
/// `initialize` logs the outcome once.
pub(crate) fn raise_open_file_limit() {
    OPEN_FILES.get_or_init(|| raise_nofile(OPEN_FILES_WANTED));
}
/// (soft before, soft after, why it stayed lower).
fn raise_nofile(wanted: u64) -> (u64, u64, Option<String>) {
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
        return (0, 0, Some(std::io::Error::last_os_error().to_string()));
    }
    let before = limit.rlim_cur as u64;
    let target = wanted.min(limit.rlim_max as u64);
    if target <= before {
        return (before, before, None);
    }
    let mut error = None;
    // macOS refuses a soft limit above kern.maxfilesperproc: step down until one is accepted.
    for soft in [target, 8192, 4096, 2048, 1024].into_iter().filter(|s| *s <= target && *s > before) {
        let raised = libc::rlimit { rlim_cur: soft as libc::rlim_t, rlim_max: limit.rlim_max };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) } == 0 {
            return (before, soft, error);
        }
        error.get_or_insert_with(|| std::io::Error::last_os_error().to_string());
    }
    (before, before, error)
}
fn open_file_limit_line() -> Option<String> {
    let (before, after, error) = OPEN_FILES.get()?;
    Some(match error {
        None if after > before => format!("open-file limit raised from {before} to {after}"),
        None => format!("open-file limit {after}"),
        Some(e) => format!("open-file limit {after} (raising from {before} failed: {e})"),
    })
}

/// Set once stdout/stderr go to `heimdall.log` (the installed app).
static LOG_CAPTURED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// The per-turn JSON event stream (each final with its decider record and 8.6 KB prompt, every agent payload, speaker
/// helper events) on stdout, for replay and CLI runs that read it. In the installed app stdout is `heimdall.log`; the
/// archive keeps all of it, so the log keeps days of problems instead of two hours (19 Oct, a call day).
pub(crate) fn event_stream(value: &Value) {
    if !LOG_CAPTURED.load(std::sync::atomic::Ordering::Relaxed) {
        println!("{value}");
    } else if let Some(text) = stream_line(value) {
        line("speakers", &text);
    }
}
/// The stream's state changes that stay in the log as plain lines: the speaker helper's model and a speaker's name.
fn stream_line(value: &Value) -> Option<String> {
    let source = value["source"].as_str().unwrap_or("?");
    Some(match value["event"].as_str()? {
        "speaker_helper_loading" => format!("speaker helper loading for {source}"),
        "speaker_helper_ready" => format!("speaker helper ready for {source}"),
        "speaker_helper_expired" => format!("speaker helper unloaded for {source}"),
        "speaker_label" => format!("{source}: {} is now {}", value["previous"].as_str().unwrap_or("unnamed"), value["speaker"].as_str().unwrap_or("?")),
        _ => return None,
    })
}

static STATE: OnceLock<Mutex<Monitor>> = OnceLock::new();
fn state() -> &'static Mutex<Monitor> {
    STATE.get_or_init(Default::default)
}
pub(crate) fn startup_record(args: &crate::Args) -> anyhow::Result<()> {
    if args.replay.is_none()
        && args.idle.is_none()
        && !args.settings_only
        && !args.hotswap_smoke
        && !args.night_replay_smoke
        && !args.night_lab_worker
    {
        let data = args
            .data
            .clone()
            .map(Ok)
            .unwrap_or_else(crate::settings::default_data)?;
        initialize(&data)?;
    }
    Ok(())
}
pub(crate) fn initialize(data: &Path) -> anyhow::Result<()> {
    host::initialize(data);
    // The first problem record finds a sample ready.
    HOST.get_or_init(|| HostCache::new(host::sample, HOST_SAMPLE_EVERY)).refresh();
    match log::capture(&data.join("logs")) {
        Ok(()) => LOG_CAPTURED.store(true, std::sync::atomic::Ordering::Relaxed),
        Err(error) => eprintln!("Durable stdout/stderr capture unavailable: {error:#}"),
    }
    heimdall_archive::diagnostics::install(heimdall_archive::diagnostics::Hooks {
        context,
        observe,
        enqueued: lines::enqueued,
    });
    eprintln!(
        "Heimdall diagnostics started {} commit={}",
        heimdall_archive::wall_ns(),
        host::commit()
    );
    eprintln!("Heimdall edition: {}", crate::edition::NAME);
    if let Some(text) = open_file_limit_line() {
        line("heimdall", &text);
    }
    Ok(())
}
/// Runs on the archive writer, inside its transaction, for every problem record: it must not scan the host there
/// (bug note 2026-11-04T02-17-22: `proc_listallpids` + `proc_pidpath` per process held the writer). It reads the
/// cached host sample, which a side thread refreshes at most every `HOST_SAMPLE_EVERY`.
fn context(r: &Record) -> Option<Value> {
    if !heimdall_archive::diagnostics::problem(&r.kind, &r.payload) {
        return None;
    }
    let runtime = state().try_lock().ok().map(|s| s.runtime.clone());
    let mut sample = HOST.get_or_init(|| HostCache::new(host::sample, HOST_SAMPLE_EVERY)).context();
    sample["runtime"] = runtime.unwrap_or_else(|| json!({"unavailable":"diagnostic state busy"}));
    Some(sample)
}
/// How old a host sample may be before a problem record asks for a new one. A burst of problems shares one scan.
const HOST_SAMPLE_EVERY: std::time::Duration = std::time::Duration::from_secs(5);
static HOST: OnceLock<HostCache> = OnceLock::new();
/// The newest host sample and when it was taken. `context` never waits for a scan: a stale or missing sample starts
/// one on its own short-lived thread and the record gets what is cached, with its age.
struct HostCache {
    scan: fn() -> Value,
    every: std::time::Duration,
    newest: std::sync::Arc<Mutex<Option<(std::time::Instant, Value)>>>,
    scanning: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl HostCache {
    fn new(scan: fn() -> Value, every: std::time::Duration) -> Self {
        Self { scan, every, newest: Default::default(), scanning: Default::default() }
    }
    fn context(&self) -> Value {
        let cached = self.newest.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let age = cached.as_ref().map(|(at, _)| at.elapsed());
        if age.is_none_or(|a| a >= self.every) {
            self.refresh();
        }
        match cached {
            Some((_, mut sample)) => {
                sample["sample_age_ms"] = json!(age.unwrap_or_default().as_millis() as u64);
                sample
            }
            None => json!({"sample_wall_ns":heimdall_archive::wall_ns(),"app_commit":host::commit(),"unavailable":"host sample still being taken"}),
        }
    }
    /// One scan at a time, off the caller's thread.
    fn refresh(&self) {
        use std::sync::atomic::Ordering;
        if self.scanning.swap(true, Ordering::AcqRel) {
            return;
        }
        let (scan, newest, scanning) = (self.scan, self.newest.clone(), self.scanning.clone());
        let started = std::thread::Builder::new().name("heimdall-host-sample".into()).spawn(move || {
            // Cleared even if the scan panics, so a later problem asks again.
            struct Done(std::sync::Arc<std::sync::atomic::AtomicBool>);
            impl Drop for Done {
                fn drop(&mut self) {
                    self.0.store(false, std::sync::atomic::Ordering::Release);
                }
            }
            let _done = Done(scanning);
            let sample = scan();
            *newest.lock().unwrap_or_else(|e| e.into_inner()) = Some((std::time::Instant::now(), sample));
        });
        if started.is_err() {
            self.scanning.store(false, Ordering::Release);
        }
    }
}
pub(crate) fn record(journal: &Writer, kind: &str, source: &str, payload: Value) {
    journal.diagnostic(kind, source, capture::host_now_ns(), payload);
}
pub(crate) fn runtime_record(
    journal: &Writer,
    s: &crate::settings::Settings,
    running: bool,
    drama_ready: bool,
) {
    let value = json!({"switches":{"Listening":!s.paused,"Seeing":!s.screen_off,"Speaking":s.speak_replies,"Agent":!s.dry_run,"Engine":s.engine_on},"sample_host_ns":capture::host_now_ns(),"agent_runtime_running":running,"drama3_ready":drama_ready});
    if let Ok(mut monitor) = state().try_lock() {
        monitor.runtime["settings"] = value;
    }
    // No evidence is emitted by an idle tick. Only newly overdue conditions enqueue.
    let now = capture::host_now_ns();
    let violations = state()
        .try_lock()
        .map(|mut s| s.expired(now))
        .unwrap_or_default();
    for payload in violations {
        record(journal, "invariant_violation", "heimdall", payload);
    }
}
pub(crate) fn ui_record(journal: &Writer, update: &ui::UiUpdate, attached: bool) {
    match update {
        ui::UiUpdate::Command(cue) => record(
            journal,
            "command_cue",
            "heimdall",
            json!({"cue":format!("{cue:?}"),"id":match cue {ui::CommandCue::Heard{id,..}|ui::CommandCue::Dropped{id}=>Some(*id),ui::CommandCue::Done{id,..}|ui::CommandCue::Failed{id,..}=>*id},"phase":match cue {ui::CommandCue::Heard{..}=>"heard",ui::CommandCue::Done{..}=>"done",ui::CommandCue::Failed{..}=>"failed",ui::CommandCue::Dropped{..}=>"dropped"},"ui_attached":attached,"stage":"dispatched"}),
        ),
        ui::UiUpdate::Health(cue) => record(
            journal,
            "health_cue",
            "heimdall",
            json!({"cue":format!("{cue:?}"),"ui_attached":attached,"stage":"dispatched"}),
        ),
        ui::UiUpdate::AgentSent => record(journal, "island_cue", "heimdall", json!({"cue":"agent_sent","ui_attached":attached,"stage":"dispatched"})),
        ui::UiUpdate::IslandHold(ms) => record(journal, "island_cue", "heimdall", json!({"cue":"hold","ms":ms,"ui_attached":attached,"stage":"dispatched"})),
        ui::UiUpdate::Vella { on, keys } => record(journal, "island_cue", "heimdall", json!({"cue":"vella","on":on,"keys":keys,"ui_attached":attached,"stage":"dispatched"})),
        ui::UiUpdate::AgentNotice(n) if n.is_line() => record(
            journal,
            "island_cue",
            "heimdall",
            json!({"cue":"agent_notice","id":bounded(&n.id,128),"kind":n.kind.name(),"text":bounded(&n.text,160),"detail":n.detail.as_ref().map(|d|match d{ui::agent_notice::Detail::Text(_)=>"text",ui::agent_notice::Detail::Table{..}=>"table",ui::agent_notice::Detail::Svg(_)=>"svg",ui::agent_notice::Detail::Image{..}=>"image"}),"ui_attached":attached,"stage":"dispatched"}),
        ),
        ui::UiUpdate::Engine { state: engine, .. } => {
            if let Ok(mut s) = state().try_lock() {
                s.runtime["splash_state"] = json!(bounded(&format!("{engine:?}"), 512));
            }
        }
        _ => (),
    }
}
pub(crate) fn command_record(
    journal: &Writer,
    cue_id: u64,
    utterance: &str,
    command: &str,
    attached: bool,
) {
    record(
        journal,
        "command_execution",
        "heimdall",
        json!({"cue_id":cue_id,"utterance_id":utterance,"command":command,"ui_attached":attached}),
    );
}
pub(crate) fn agent_record(journal: &Writer, window: &ui::AgentWindowState) {
    let payload = json!({"state":format!("{:?}",window.state),"queue_depth":window.queued,"unresolved":window.unresolved,"problem":window.problem});
    let signature = heimdall_archive::hash(&serde_json::to_vec(&payload).unwrap_or_default());
    let snapshot = json!({"state":format!("{:?}",window.state),"queue_depth":window.queued,"unresolved_count":window.unresolved.len(),"unresolved":window.unresolved.iter().take(16).map(|id|bounded(id,64)).collect::<Vec<_>>(),"problem":window.problem.as_ref().map(|p|bounded(p,512)),"details_truncated":window.unresolved.len()>16 || window.unresolved.iter().any(|id|id.len()>64) || window.problem.as_ref().is_some_and(|p|p.len()>512)});
    let changed = state()
        .try_lock()
        .map(|mut s| {
            let changed = s.agent_signature.as_ref() != Some(&signature);
            s.agent_signature = Some(signature);
            s.runtime["agent"] = snapshot;
            changed
        })
        .unwrap_or(false);
    if changed {
        record(journal, "agent_state", "agent:pi", payload);
    }
}
fn bounded(text: &str, limit: usize) -> &str {
    let mut end = text.len().min(limit);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
pub(crate) fn health(journal: &Writer, cue: &ui::HealthCue, reason: Option<&str>) {
    let (component, state) = match cue {
        ui::HealthCue::Restarting { key, .. } => (key.as_str(), "restarting"),
        ui::HealthCue::Busy { key, .. } => (key.as_str(), "busy"),
        ui::HealthCue::Recovered { key, .. } => (key.as_str(), "recovered"),
        ui::HealthCue::Problem { key, .. } => (key.as_str(), "problem"),
        ui::HealthCue::Drop { key, .. } => (key.as_str(), "unavailable"),
        ui::HealthCue::Clear { key } => (key.as_str(), "cleared"),
        ui::HealthCue::Notice { .. } => ("notice", "notice"),
    };
    record(
        journal,
        "health",
        "heimdall",
        json!({"component": component, "state": state, "cue": format!("{cue:?}"), "reason": reason}),
    );
}
#[derive(Default)]
struct Monitor {
    runtime: Value,
    agent_signature: Option<String>,
    // Bounded, transition-only conditions. Evidence ids tie every check to its inputs.
    signals: BTreeMap<String, (u64, Vec<i64>, bool)>,
    commands: BTreeMap<u64, (u64, i64)>,
    orphan_cues: BTreeMap<u64, (u64, i64)>,
    conditions: BTreeSet<String>,
    health_records: BTreeMap<String, i64>,
}
impl Monitor {
    fn violation(&mut self, key: String, active: bool, evidence: Value) -> Option<Value> {
        if !active {
            self.conditions.remove(&key);
            return None;
        }
        if self.conditions.len() >= 256 {
            self.conditions.clear();
        }
        self.conditions
            .insert(key.clone())
            .then(|| json!({"check":key,"evidence":evidence}))
    }
    fn expired(&mut self, now: u64) -> Vec<Value> {
        let mut out:Vec<Value>=self.signals.values_mut().filter_map(|(at,ids,reported)|{
            if !*reported && now.saturating_sub(*at)>5_000_000_000 {*reported=true;Some(json!({"check":"completion_not_settled","threshold_seconds":5,"signal_evidence_ids":ids,"elapsed_ns":now.saturating_sub(*at)}))}else{None}
        }).collect();
        for (check, map) in [
            ("command_without_cue_dispatch", &mut self.commands),
            ("cue_dispatch_without_command", &mut self.orphan_cues),
        ] {
            let expired: Vec<_> = map
                .iter()
                .filter(|(_, (at, _))| now.saturating_sub(*at) > 5_000_000_000)
                .map(|(id, _)| *id)
                .collect();
            for cue in expired {
                if let Some((at, id)) = map.remove(&cue) {
                    out.push(json!({"check":check,"cue_id":cue,"evidence_id":id,"elapsed_ns":now.saturating_sub(at),"stage":"dispatched, visibility not established"}));
                }
            }
        }
        out
    }

    fn observe(&mut self, r: &Record, id: i64) -> Vec<Value> {
        let p = &r.payload;
        if ["timeline_settings", "settings_applied"].contains(&r.kind.as_str()) {
            let s = &p["settings"];
            self.runtime["settings"] = json!({"sample_host_ns":r.host_ns,"switches":{"Listening":s["paused"]==false,"Seeing":s["screen_off"]==false,"Speaking":s["speak_replies"],"Agent":s["dry_run"]==false,"Engine":s["engine_on"]}});
        }
        let mut out = vec![];
        let mut add = |v: Option<Value>| {
            if let Some(v) = v {
                out.push(v)
            }
        };
        if [
            "decision",
            "decider_owner_failed",
            "d1_tcl",
            "d1_question",
            "send_decision",
            "command_decision",
            "voice_reply",
            "drama3",
            "command_action",
        ]
        .contains(&r.kind.as_str())
        {
            let bad = heimdall_archive::diagnostics::problem(&r.kind, p);
            add(self.violation(format!("{}_problem:{}",r.kind,r.source),bad,json!({"evidence_id":id,"utterance_ids":r.utterance_ids,"error":p["error"],"event":p["event"],"available_bytes":p["available_bytes"],"required_available_bytes":p["required_available_bytes"]})));
        }
        if r.kind=="send_decision" {
            add(self.violation(format!("send_decision_slow:{}",r.source),p["latency_ms"].as_f64().is_some_and(|ms|ms>500.),json!({"evidence_id":id,"latency_ms":p["latency_ms"],"limit_ms":500})));
            add(self.violation(format!("send_decision_window:{}",r.source),p["input_tokens"].as_u64().is_some_and(|n|n>2000),json!({"evidence_id":id,"input_tokens":p["input_tokens"],"limit_tokens":2000})));
        }
        if r.kind == "health" {
            if let Some(cue) = p["cue"].as_str() {
                if self.health_records.len() >= 256 {
                    self.health_records.clear();
                }
                self.health_records.insert(cue.into(), id);
            }
        }
        if r.kind == "health_cue" {
            if let Some(cue) = p["cue"].as_str() {
                if self.health_records.remove(cue).is_none() {
                    add(Some(
                        json!({"check":"health_cue_without_record","evidence_id":id,"cue":cue,"stage":"dispatched"}),
                    ));
                }
            }
        }
        if r.kind == "agent_state" {
            add(self.violation("agent_queue_depth".into(),p["queue_depth"].as_u64().unwrap_or(0)>1,json!({"evidence_id":id,"queue_depth":p["queue_depth"],"unresolved":p["unresolved"]})));
        }
        if r.kind == "agent_transport_reply" {
            let w = &p["wire"];
            if w["type"] == "agent_end"
                || w["type"] == "entry_appended"
                    && w["entry"]["customType"] == "heimdall_receipt"
                    && w["entry"]["data"]["completed"] == true
            {
                let key = w["entry"]["data"]["id"]
                    .as_str()
                    .unwrap_or("active")
                    .to_owned();
                if self.signals.len() < 128 {
                    let active = if key != "active" {
                        self.signals.remove("active")
                    } else {
                        None
                    };
                    let signal = self
                        .signals
                        .entry(key)
                        .or_insert_with(|| active.unwrap_or((r.host_ns, vec![], false)));
                    if signal.1.len() < 4 {
                        signal.1.push(id);
                    }
                }
            }
            // agent_settled is only a transport signal. Exact complete below proves ledger settlement.
        }
        if r.kind == "agent_reply"
            && p["event"]["kind"] == "complete"
            && p["attribution"] == "exact"
        {
            if let Some(key) = p["event"]["id"].as_str() {
                self.signals.remove(key);
            }
            self.signals.remove("active");
        }
        if r.kind == "command_execution" {
            if let Some(cue) = p["cue_id"].as_u64() {
                if self.orphan_cues.remove(&cue).is_none() && self.commands.len() < 256 {
                    self.commands.insert(cue, (r.host_ns, id));
                }
                add(self.violation(
                    format!("command_ui_unavailable:{cue}"),
                    p["ui_attached"] == false,
                    json!({"evidence_id":id,"cue_id":cue}),
                ));
            }
        }
        if r.kind == "command_cue"
            && ["done", "failed"].contains(&p["phase"].as_str().unwrap_or(""))
        {
            if let Some(cue) = p["id"].as_u64().filter(|id| *id != u64::MAX) {
                let matched = self.commands.remove(&cue).is_some();
                if !matched && p["phase"] == "done" && self.orphan_cues.len() < 256 {
                    self.orphan_cues.insert(cue, (r.host_ns, id));
                }
            }
        }
        // Archive rows and raw wake inputs precede attachment deduplication.
        // Only the outbound envelope proves what was actually attached.
        if r.kind == "agent_send" {
            if let Some(content) = p["payload"]["message"]["content"].as_array() {
                let images: Vec<_> = content.iter()
                    .filter(|part| part["type"] == "image")
                    .filter_map(|part| part["source"]["archive_image_id"].as_str())
                    .collect();
                let unique: BTreeSet<_> = images.iter().copied().collect();
                add(self.violation(
                    format!("duplicate_screenshot:{id}"),
                    unique.len() < images.len(),
                    json!({"evidence_id":id,"message_id":p["message_id"],"image_ids":images}),
                ));
            }
        }
        out
    }
}
fn observe(r: &Record, id: i64) -> Vec<Record> {
    let mut monitor = match state().try_lock() {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    monitor
        .observe(r, id)
        .into_iter()
        .map(|payload| Record {
            kind: "invariant_violation".into(),
            source: r.source.clone(),
            utterance_ids: r.utterance_ids.clone(),
            host_ns: r.host_ns,
            wall_ns: r.wall_ns,
            payload,
        })
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    static SCANS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    fn slow_scan() -> Value {
        SCANS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(150));
        json!({"sample_wall_ns":1u64,"app_commit":"test"})
    }
    /// Bug note 2026-11-04T02-17-22: a problem record never scans the host on the archive writer, and a burst of them
    /// shares one scan.
    #[test]
    fn problem_context_never_waits_for_a_host_scan_and_a_burst_shares_one() {
        let cache = HostCache::new(slow_scan, std::time::Duration::from_secs(60));
        let start = std::time::Instant::now();
        let first = cache.context();
        assert!(first["sample_wall_ns"].is_u64() && first["app_commit"].is_string(), "{first}");
        for _ in 0..100 {
            cache.context();
        }
        assert!(start.elapsed() < std::time::Duration::from_millis(100), "waited for the scan: {:?}", start.elapsed());
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while cache.newest.lock().unwrap().is_none() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let cached = cache.context();
        assert_eq!(cached["app_commit"], "test");
        assert!(cached["sample_age_ms"].is_u64());
        assert_eq!(SCANS.load(std::sync::atomic::Ordering::SeqCst), 1, "one scan for the whole burst");
        // Measured on this Mac: what the archive writer paid per problem record before, and pays now.
        let real = HostCache::new(host::sample, HOST_SAMPLE_EVERY);
        let t = std::time::Instant::now();
        host::sample();
        let scan = t.elapsed();
        real.context();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let t = std::time::Instant::now();
        for _ in 0..100 {
            real.context();
        }
        eprintln!("host scan {scan:?} per problem record before; cached context {:?} each now", t.elapsed() / 100);
    }
    /// 26 Oct: the app ran with launchd's 256 open files. The limit is raised (only ever raised, never past the hard
    /// limit), and a wanted limit at or below the current one changes nothing.
    #[test]
    fn the_open_file_limit_is_raised_toward_the_wanted_one() {
        let mut now = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut now) }, 0);
        let soft = now.rlim_cur as u64;
        assert_eq!(raise_nofile(soft), (soft, soft, None));
        assert_eq!(raise_nofile(soft / 2), (soft, soft, None));
        if (now.rlim_max as u64) >= soft + 64 && soft + 64 <= OPEN_FILES_WANTED {
            assert_eq!(raise_nofile(soft + 64), (soft, soft + 64, None));
            assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut now) }, 0);
            assert_eq!(now.rlim_cur as u64, soft + 64);
        }
    }
    fn r(kind: &str, payload: Value, host_ns: u64) -> Record {
        Record {
            kind: kind.into(),
            source: "test".into(),
            utterance_ids: vec!["turn".into()],
            host_ns,
            wall_ns: host_ns,
            payload,
        }
    }
    #[test]
    fn slow_send_decisions_generate_a_named_invariant() {
        let mut monitor=Monitor::default();
        let out=monitor.observe(&r("send_decision",json!({"latency_ms":501,"input_tokens":1900,"error":null}),1),42);
        assert!(out.iter().any(|p|p["check"]=="send_decision_slow:test"));
        let out=monitor.observe(&r("decider_owner_failed",json!({"error":"owner panicked","attempt":1}),2),43);
        assert!(out.iter().any(|p|p["check"]=="decider_owner_failed_problem:test"));
    }
    #[test]
    fn durable_child_fixture() {
        let Some(root) = std::env::var_os("HEIMDALL_DIAGNOSTIC_TEST_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        initialize(&root).unwrap();
        println!("stdout durable sentinel");
        eprintln!("stderr durable sentinel");
        event_stream(&json!({"event":"final","text":"event stream sentinel"}));
        let session = root.join("archive/session-diagnostic-test");
        let journal = heimdall_archive::Journal::open(&session).unwrap();
        journal
            .event(
                "voice_reply",
                "mic",
                &["turn".into()],
                capture::host_now_ns(),
                json!({"error":"reply question requires audio"}),
            )
            .unwrap();
        let writer = Writer::open(&session).unwrap();
        record(
            &writer,
            "health",
            "heimdall",
            json!({"state":"problem","reason":"synthetic archive failure"}),
        );
        writer.flush().unwrap();
        if std::env::var_os("HEIMDALL_DIAGNOSTIC_TEST_PANIC").is_some() {
            panic!("panic durable sentinel");
        }
    }
    #[test]
    fn finder_style_output_panic_and_both_journal_paths_are_durable() {
        for panic in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "diagnostics::tests::durable_child_fixture",
                    "--nocapture",
                ])
                .env("HEIMDALL_DIAGNOSTIC_TEST_ROOT", temp.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if panic {
                command.env("HEIMDALL_DIAGNOSTIC_TEST_PANIC", "1");
            }
            let mut child = command.spawn().unwrap();
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let status = loop {
                if let Some(s) = child.try_wait().unwrap() {
                    break s;
                }
                if std::time::Instant::now() > until {
                    let _ = child.kill();
                    panic!("diagnostic child deadline");
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            };
            assert_eq!(status.success(), !panic);
            let log = std::fs::read_to_string(temp.path().join("logs/heimdall.log")).unwrap();
            assert!(log.contains("stdout durable sentinel"));
            assert!(log.contains("stderr durable sentinel"));
            // Every line has its local time; both journal paths' problems are plain lines (night 20 Oct).
            for line in log.lines().filter(|l| !l.is_empty()) {
                assert!(line.len() > 24 && line.as_bytes()[4] == b'-' && line.as_bytes()[13] == b':', "{line}");
            }
            assert!(log.contains("health: ? problem: synthetic archive failure"), "{log}");
            assert!(log.contains("speech: voice_reply (mic): error: reply question requires audio"), "{log}");
            // The per-turn JSON stream stays out of the log.
            assert!(!log.contains("event stream sentinel"), "{log}");
            if panic {
                assert!(log.contains("panic durable sentinel"));
            }
            let rows = heimdall_archive::Journal::open_read_only(
                &temp.path().join("archive/session-diagnostic-test"),
            )
            .unwrap()
            .records()
            .unwrap();
            for kind in ["voice_reply", "health", "invariant_violation"] {
                let row = rows.iter().find(|r| r.kind == kind).unwrap();
                assert!(
                    row.payload["system_context"]["sample_wall_ns"].is_u64(),
                    "{kind}"
                );
                assert!(row.payload["system_context"]["app_commit"].is_string());
            }
        }
    }
    #[test]
    fn agent_state_diagnostic_does_not_wait_for_an_archive_write_lock() {
        let temp = tempfile::tempdir().unwrap();
        let session = temp.path().join("session-state");
        let journal = heimdall_archive::Journal::open(&session).unwrap();
        let writer = Writer::open(&session).unwrap();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let db_path = journal.root().join(heimdall_archive::store::INDEX);
        let lock = std::thread::spawn(move || {
            let db = rusqlite::Connection::open(db_path).unwrap();
            db.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(std::time::Duration::from_millis(500));
            db.execute_batch("ROLLBACK").unwrap();
        });
        locked_rx.recv().unwrap();
        let window = ui::AgentWindowState { state: ui::AgentRunState::Working, can_send: true, can_stop: true, can_reload: true,
            problem: None, unresolved: vec!["state-lock-regression".into()], queued: 0, reviews: vec![], reload: None };
        let start = std::time::Instant::now();
        agent_record(&writer, &window);
        let elapsed = start.elapsed();
        let _ = release_tx.send(());
        lock.join().unwrap();
        writer.flush().unwrap();
        assert!(elapsed < std::time::Duration::from_millis(200), "diagnostic waited for archive: {elapsed:?}");
        assert!(journal.records().unwrap().iter().any(|r| r.kind == "agent_state" && r.payload["unresolved"][0] == "state-lock-regression"));
    }
    #[test]
    fn identical_archived_captures_are_not_duplicate_attachments() {
        let mut m = Monitor::default();
        let capture = r("screenshot", json!({"image_id":"same-content"}), 1);
        assert!(m.observe(&capture, 1).is_empty());
        assert!(m.observe(&capture, 2).is_empty(), "two archive rows do not prove two attachments");
        // Raw wake input can contain duplicates which the bridge removes before send.
        assert!(m.observe(&r("agent_wake", json!({"image_ids":["same-content","same-content"]}), 2), 3).is_empty());
        let send = r("agent_send", json!({"payload":{"message":{"content":[
            {"type":"image","source":{"archive_image_id":"same-content"}}
        ]}}}), 3);
        assert!(m.observe(&send, 4).is_empty());
    }
    #[test]
    fn real_tool_turn_completion_without_exact_settlement_reports_once() {
        let mut m = Monitor::default();
        m.observe(
            &r(
                "agent_transport_reply",
                json!({"wire":{"type":"agent_end"}}),
                1,
            ),
            10,
        );
        m.observe(&r("agent_transport_reply",json!({"wire":{"type":"entry_appended","entry":{"customType":"heimdall_receipt","data":{"id":"ce85c029","completed":true}}}}),2),11);
        assert!(m.expired(5_000_000_001).is_empty());
        assert_eq!(m.expired(6_000_000_000).len(), 1);
        assert!(m.expired(9_000_000_000).is_empty());
        m.observe(
            &r(
                "agent_reply",
                json!({"event":{"kind":"complete","id":"ce85c029"},"attribution":"exact"}),
                10_000_000_000,
            ),
            12,
        );
        assert!(m.signals.is_empty());
    }
    #[test]
    fn voice_error_and_queue_are_transitions_not_tick_spam() {
        let mut m = Monitor::default();
        let e = r(
            "voice_reply",
            json!({"error":"reply question requires audio"}),
            1,
        );
        assert_eq!(m.observe(&e, 1).len(), 1);
        assert!(m.observe(&e, 2).is_empty());
        m.observe(&r("voice_reply", json!({"error":null}), 2), 3);
        assert_eq!(m.observe(&e, 4).len(), 1);
        let q = r(
            "agent_state",
            json!({"queue_depth":7,"unresolved":["ce85c029"]}),
            3,
        );
        assert_eq!(m.observe(&q, 5).len(), 1);
        assert!(m.observe(&q, 6).is_empty());
        assert!(
            m.observe(&r("agent_state", json!({"queue_depth":0}), 4), 7)
                .is_empty()
        );
        assert_eq!(m.observe(&q, 8).len(), 1);
    }
    #[test]
    fn command_and_health_dispatch_checks_match_evidence_and_expire_once() {
        let mut m = Monitor::default();
        m.observe(
            &r(
                "command_execution",
                json!({"cue_id":42,"ui_attached":true}),
                1,
            ),
            1,
        );
        m.observe(&r("command_cue", json!({"id":42,"phase":"done"}), 2), 2);
        assert!(m.expired(6_000_000_000).is_empty());
        m.observe(
            &r(
                "command_execution",
                json!({"cue_id":43,"ui_attached":true}),
                3,
            ),
            3,
        );
        let v = m.expired(6_000_000_000);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["check"], "command_without_cue_dispatch");
        assert!(m.expired(9_000_000_000).is_empty());
        m.observe(&r("command_cue", json!({"id":44,"phase":"done"}), 4), 4);
        assert_eq!(
            m.expired(6_000_000_000)[0]["check"],
            "cue_dispatch_without_command"
        );
        m.observe(&r("health", json!({"cue":"Problem one"}), 5), 5);
        assert!(
            m.observe(&r("health_cue", json!({"cue":"Problem one"}), 6), 6)
                .is_empty()
        );
        assert_eq!(
            m.observe(&r("health_cue", json!({"cue":"Problem missing"}), 7), 7)[0]["check"],
            "health_cue_without_record"
        );
    }
    #[test]
    fn runtime_strings_have_byte_bounds_without_splitting_utf8() {
        let text = "🛠".repeat(1000);
        let clipped = bounded(&text, 513);
        assert_eq!(clipped.len(), 512);
        assert_eq!(clipped.chars().count(), 128);
    }
    #[test]
    fn repeated_screenshot_ids_in_one_outbound_envelope_are_evidence() {
        let mut m = Monitor::default();
        assert_eq!(
            m.observe(
                &r("agent_send", json!({"message_id":"wake","payload":{"message":{"content":[
                    {"type":"image","source":{"archive_image_id":"one"}},
                    {"type":"image","source":{"archive_image_id":"two"}},
                    {"type":"image","source":{"archive_image_id":"one"}}
                ]}}}), 1),
                22
            )
            .len(),
            1
        );
    }
}
#[cfg(test)]
mod stream_tests {
    use super::*;
    #[test]
    fn the_event_stream_keeps_only_state_changes_as_log_lines() {
        assert_eq!(stream_line(&json!({"event":"speaker_helper_ready","source":"process:1"})).as_deref(), Some("speaker helper ready for process:1"));
        assert_eq!(stream_line(&json!({"event":"speaker_label","source":"Discord","previous":"Speaker 1","speaker":"Angel"})).as_deref(), Some("Discord: Speaker 1 is now Angel"));
        assert_eq!(stream_line(&json!({"event":"final","decision":{"prompt":"x".repeat(8600)}})), None);
        assert_eq!(stream_line(&json!({"dry_run":true,"would_send":{}})), None);
    }
}
