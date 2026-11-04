//! Resilience inside the live loop (docs/resilience.md): each part's health on the island, the real-freeze checks
//! for capture and the listen worker, the lower rungs of the recovery ladder, the microphone switch cue, the
//! decider's load and inference health, the archive and disk, and the agent pre-flight. main.rs owns capture, the
//! screen stream and relaunching; it carries out the `Recovery` this asks for.

use super::*;
use crate::health::{MicCue, Recovery, Rung};
use agent::delivery::StopReason;
use std::time::Duration;
use ui::{HealthControl, Part};

/// Answers from helper threads to the live loop.
pub enum Internal {
    Inference(super::send_worker::Completion),
    Preflight { id: u64, result: std::result::Result<String, crate::preflight::Failure> },
}

/// An open microphone that delivers no audio this long is stalled. Capture restarts a silent device after 3 s by
/// itself; this catches capture itself stuck. A quiet room still delivers audio (silence is samples), so quiet is
/// never a stall, and only microphones count: an app tap is silent whenever the app is.
pub const CAPTURE_STALL: Duration = Duration::from_secs(15);
/// The stall clock of a restarted capture until one of its microphones opens (`capture_restarting`).
const RESTARTING_MIC: &str = "mic:restarting";
/// The listen worker holding queued work without finishing any of it for this long is hung.
pub const WORKER_STALL: Duration = Duration::from_secs(30);
/// Loading the listen models for longer than this is hung (a normal load takes 2-5 s).
pub const WORKER_LOAD_STALL: Duration = Duration::from_secs(120);
/// The live loop ticks every 2 s; a longer gap means it was busy, and capture is not judged on that round.
const TICK_GAP: Duration = Duration::from_secs(6);

impl Pipeline {
    /// Sends a health cue immediately and enqueues its diagnostic without waiting
    /// for the archive. Avoid the event index disk check, which resets the status it reports on.
    pub(crate) fn health_cue(&mut self, cue: Option<ui::HealthCue>) {
        let Some(cue) = cue else { return };
        // A capture cue's log line names the input in use, so "is the mic live" is answered from the log alone (bug
        // note 2026-10-27T00-47-00).
        let capture = matches!(&cue, ui::HealthCue::Restarting { key, .. } | ui::HealthCue::Recovered { key, .. }
            | ui::HealthCue::Problem { key, .. } | ui::HealthCue::Drop { key, .. } | ui::HealthCue::Clear { key } if key == "capture");
        let inputs = capture.then(|| self.inputs_in_use());
        crate::diagnostics::health(&self.evidence, &cue, inputs.as_deref());
        self.ui(ui::UiUpdate::Health(cue));
    }

    /// Screen owner events are state transitions, including their raw cause. A recreated
    /// stream is as recovered as the initial stream; zero displays is never healthy.
    pub(crate) fn screen_event(&mut self, event: screen::ScreenEvent) {
        let description = format!("{event:?}");
        if self.last_screen_event.as_ref() == Some(&description) { return; }
        self.last_screen_event = Some(description.clone());
        crate::diagnostics::record(&self.evidence, "health", "heimdall", serde_json::json!({
            "component":"screen", "state":"transition", "reason":description,
        }));
        let part = Part::Screen;
        match &event {
            screen::ScreenEvent::Unavailable => self.screen_gave_up = true,
            screen::ScreenEvent::Started { displays } | screen::ScreenEvent::DisplaysChanged { displays } if *displays > 0 => self.screen_gave_up = false,
            _ => {}
        }
        let (cue, status) = match event {
            screen::ScreenEvent::Configured { error } => {
                self.settings_result("Screen Recording", error);
                // A successful settings acknowledgement does not mean capture recovered.
                return;
            },
            screen::ScreenEvent::Started { displays } | screen::ScreenEvent::DisplaysChanged { displays } if displays > 0 => {
                (self.health.recovered("screen", part, "Screen capture OK"), None)
            },
            screen::ScreenEvent::Started { .. } | screen::ScreenEvent::DisplaysChanged { .. } => (
                self.health.dropped("screen", part, "Screen capture paused · no active display"),
                Some("Screen unavailable · no active display".into()),
            ),
            screen::ScreenEvent::Paused { reason } => (
                self.health.dropped("screen", part, "Screen capture paused · display asleep or locked"),
                Some(format!("Screen paused: {reason}")),
            ),
            screen::ScreenEvent::Recovering { attempt, delay_seconds } => (
                self.health.restarting("screen", part, "Restarting screen capture…"),
                Some(format!("Screen retry {attempt} in {delay_seconds}s")),
            ),
            screen::ScreenEvent::Unavailable => (
                self.health.dropped("screen", part, "Screen capture stopped · waiting for the display"),
                Some("Screen unavailable · retries exhausted; waiting for wake, a display change or the archive catching up".into()),
            ),
            screen::ScreenEvent::Error(error) => {
                self.note_line(&format!("Screen: {error}"));
                (self.health.restarting("screen", part, "Restarting screen capture…"), Some("Screen recovering".into()))
            },
        };
        self.health_cue(cue);
        self.component_status("screen", status);
    }

    pub(crate) fn screen_stopped(&mut self, reason: &str) {
        self.screen_gave_up = false;
        if self.last_screen_event.as_deref() != Some(reason) {
            crate::diagnostics::record(&self.evidence, "health", "heimdall", serde_json::json!({"component":"screen", "state":"stopped", "reason":reason}));
            self.last_screen_event = Some(reason.into());
        }
        let cue = self.health.clear("screen");
        self.health_cue(cue);
        self.component_status("screen", None);
    }

    pub fn internal_events(&self) -> crossbeam_channel::Receiver<Internal> {
        self.internal.1.clone()
    }

    /// The recovery main.rs must carry out, if any (the strongest asked since the last call).
    pub fn take_recovery(&mut self) -> Option<Recovery> {
        self.recovery.take()
    }

    pub(crate) fn request_recovery(&mut self, r: Recovery) {
        let rank = |r: &Recovery| match r {
            Recovery::RestartCapture(_) | Recovery::RestartScreen(_) => 0,
            Recovery::RestartPipeline(_) => 1,
            Recovery::Relaunch { .. } => 2,
        };
        if self.recovery.as_ref().is_none_or(|old| rank(&r) >= rank(old)) {
            if let Err(e) = self.index.evidence.event("recovery", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"recovery": format!("{r:?}")})) {
                eprintln!("Recovery not archived: {e:#}");
            }
            self.recovery = Some(r);
        }
    }

    /// The live loop's 2-second tick: freeze checks and delayed cues. `since_last` is the time since the previous
    /// tick was handled.
    pub fn health_tick(&mut self, since_last: Duration, listen_tx: &crossbeam_channel::Sender<Option<listen::WorkerEvent>>) {
        crate::diagnostics::runtime_record(&self.evidence, &self.settings, self.agent.as_ref().is_some_and(|a| a.is_running()), self.tools.drama3.ready());
        if let Err(e) = self.call_tick() {
            self.fail(&format!("Call transcript update failed: {e:#}"));
        }
        self.voice_namer_tick();
        self.heimdall_voice_tick();
        self.suggest_tick();
        self.inference_engine_events();
        let now = Instant::now();
        if let Some(cue) = self.mics.tick(now) {
            self.mic_cue(cue);
        }
        if self.listening {
            if since_last > TICK_GAP {
                // This loop was busy: audio may be queued behind it. Judge capture from now on.
                for t in self.mic_audio.values_mut() {
                    *t = now;
                }
            } else {
                self.check_capture(now);
            }
            self.check_worker(now, listen_tx);
            self.check_agent(now);
        }
        // After a relaunch: "Heimdall OK" once it runs again (listening and transcribing, or simply up when off).
        if self.relaunched.is_some() && (!self.listening || self.live_worker.as_ref().is_some_and(|w| w.is_ready())) {
            let reason = self.relaunched.take();
            let _ = self.index.evidence.event("relaunched", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"reason": reason}));
            self.ui(ui::UiUpdate::Health(ui::HealthCue::Recovered { key: "heimdall".into(), part: Part::Heimdall, label: "Heimdall OK".into() }));
        }
    }

    fn check_capture(&mut self, now: Instant) {
        let Some((id, last)) = self.mic_audio.iter().find(|(_, t)| now.duration_since(**t) > CAPTURE_STALL).map(|(i, t)| (i.clone(), *t)) else {
            return;
        };
        let silent_ms = now.duration_since(last).as_millis() as u64;
        let name = self.sources.get(&id).map_or_else(|| "The microphone".to_string(), super::display_name);
        // Counted once per stall: the clocks restart with the recovery.
        for t in self.mic_audio.values_mut() {
            *t = now;
        }
        let rung = self.capture_ladder.climb(now);
        let _ = self.index.evidence.event("capture_stalled", &id, &[], capture::host_now_ns(), serde_json::json!({"silent_ms": silent_ms, "rung": format!("{rung:?}")}));
        self.note_line(&format!("{name} stopped delivering audio; restarting capture"));
        let cue = self.health.restarting("capture", Part::Microphone, "Restarting the microphone…");
        self.health_cue(cue);
        self.request_recovery(match rung {
            Rung::Part => Recovery::RestartCapture(format!("{id} silent for {silent_ms} ms")),
            Rung::Pipeline => Recovery::RestartPipeline(format!("{id} silent again ({silent_ms} ms)")),
            Rung::Relaunch => Recovery::Relaunch { key: "capture".into(), part: Part::Microphone, problem: "The microphone keeps stopping".into() },
        });
    }

    /// Audio from a microphone reached this loop (the capture-stall clock), and a stalled capture is back.
    pub(crate) fn mic_audio_arrived(&mut self, id: &str) {
        if let Some(t) = self.mic_audio.get_mut(id) {
            *t = Instant::now();
            if self.health.holds("capture") {
                let cue = self.health.recovered("capture", Part::Microphone, "Microphone OK");
                self.health_cue(cue);
            }
        }
    }

    /// The microphones capture has open now, for the capture health line: "input: Shure MV7i".
    pub(crate) fn inputs_in_use(&self) -> String {
        let mut names: Vec<&str> = self.sources.values()
            .filter(|i| matches!(i.kind, capture::SourceKind::InputDevice { .. }))
            .map(|i| i.label.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        if names.is_empty() { "input: none open".into() } else { format!("input: {}", names.join(", ")) }
    }

    /// Capture restarts (the live loop's `RestartCapture`): the stopped run's microphones stop off the loop and the new
    /// run's come back under new ids (bug note 2026-10-27T16-57-38). Their clocks go; one stands in for the new run
    /// until a microphone of it opens, so a restart that brings none back still climbs the ladder.
    pub fn capture_restarting(&mut self) {
        let any = !self.mic_audio.is_empty();
        self.mic_audio.clear();
        if any {
            self.mic_audio.insert(RESTARTING_MIC.into(), Instant::now());
        }
    }

    pub(crate) fn mic_source(&mut self, info: &capture::SourceInfo, open: bool) {
        if !matches!(info.kind, capture::SourceKind::InputDevice { .. }) {
            return;
        }
        if open {
            self.mic_audio.remove(RESTARTING_MIC);
            self.mic_audio.insert(info.id.0.clone(), Instant::now());
        } else {
            self.mic_audio.remove(&info.id.0);
        }
    }

    fn check_worker(&mut self, now: Instant, tx: &crossbeam_channel::Sender<Option<listen::WorkerEvent>>) {
        let Some(worker) = self.live_worker.clone() else {
            self.worker_watch = None;
            return;
        };
        let p = worker.progress();
        if p.finished {
            // Its end marker restarts it (worker_ended).
            return;
        }
        if !p.ready {
            if self.worker_spawned.is_some_and(|t| now.duration_since(t) > WORKER_LOAD_STALL) {
                self.abandon_worker("the listen models did not load within 2 minutes", tx);
            }
            return;
        }
        if p.waiting == 0 {
            // Nothing to do is quiet, not stuck.
            self.worker_watch = None;
            return;
        }
        match self.worker_watch {
            Some((done, since)) if done == p.done => {
                if now.duration_since(since) > WORKER_STALL {
                    self.abandon_worker(&format!("no work finished for {} s with {} waiting", now.duration_since(since).as_secs(), p.waiting), tx);
                }
            }
            _ => self.worker_watch = Some((p.done, now)),
        }
    }

    /// A hung listen worker cannot be stopped (it is inside a model call); it is left behind, its output
    /// discarded, and a new worker starts, within the same bound as a worker that ended (3 in 10 minutes). Its
    /// models stay in memory until Heimdall exits, which is why the bound leads to a relaunch.
    pub(crate) fn abandon_worker(&mut self, why: &str, tx: &crossbeam_channel::Sender<Option<listen::WorkerEvent>>) {
        let now = Instant::now();
        while self.worker_restarts.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(600)) {
            self.worker_restarts.pop_front();
        }
        let restarting = self.worker_restarts.len() < 3;
        let _ = self.index.evidence.event("listen_worker_hung", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"why": why, "restarting": restarting}));
        if let Some(stop) = self.live_stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(worker) = self.live_worker.take() {
            worker.request_shutdown();
        }
        self.correction_worker_ended();
        self.worker_watch = None;
        let ids: Vec<_> = self.sources.keys().cloned().collect();
        for id in &ids {
            self.gap(id, capture::host_now_ns(), "transcription restarted");
        }
        self.note_line(&format!("Transcription stuck ({why}); restarting it"));
        if restarting {
            self.worker_restarts.push_back(now);
            let cue = self.health.restarting("listen", Part::Transcription, "Restarting transcription…");
            self.health_cue(cue);
            if let Err(e) = self.enable_live_worker(tx.clone()) {
                self.fail(&format!("Listen: {e:#}"));
            }
        } else {
            self.request_recovery(Recovery::Relaunch { key: "listen".into(), part: Part::Transcription, problem: "Transcription keeps stopping".into() });
        }
    }

    /// The agent runtime died (its thread ended) while it should run: start it again; past the ladder, a problem.
    fn check_agent(&mut self, now: Instant) {
        if !self.agent.as_ref().is_some_and(|a| !a.is_running()) {
            return;
        }
        let rung = self.agent_ladder.climb(now);
        let _ = self.index.evidence.event("agent_runtime_ended", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"rung": format!("{rung:?}")}));
        if rung == Rung::Relaunch {
            self.agent = None;
            let cue = self.health.problem("agent-runtime", Part::Agent, "The agent keeps stopping", "Click to restart Heimdall", HealthControl::Relaunch);
            self.health_cue(cue);
            return;
        }
        let cue = self.health.restarting("agent-runtime", Part::Agent, "Restarting the agent…");
        self.health_cue(cue);
        self.agent = None;
        match self.restart_agent() {
            Ok(()) => {
                let cue = self.health.recovered("agent-runtime", Part::Agent, "Agent OK");
                self.health_cue(cue);
            }
            Err(e) => self.fail(&format!("Agent restart failed: {e:#}")),
        }
    }

    /// Capture's microphone report: the switch to the built-in microphone and back, or no microphone at all.
    pub(crate) fn mic_health(&mut self, state: &capture::MicrophoneState) {
        if !self.listening {
            return;
        }
        let chosen_name = self.settings.microphone.as_ref().map(|m| m.name.clone());
        for cue in self.mics.update(state, chosen_name.as_deref(), Instant::now()) {
            self.mic_cue(cue);
        }
    }

    fn mic_cue(&mut self, cue: MicCue) {
        let _ = self.index.evidence.event("microphone_cue", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"cue": format!("{cue:?}")}));
        let cue = match cue {
            MicCue::Switched(label) => Some(ui::HealthCue::Notice { part: Part::Microphone, label }),
            MicCue::Missing => self.health.problem("mic", Part::Microphone, "No microphone", "Click to pick one in the menu", HealthControl::OpenMenu),
            MicCue::Found => self.health.recovered("mic", Part::Microphone, "Microphone OK"),
            MicCue::ExtraGone(uid, name) => self.health.dropped(&format!("mic:{uid}"), Part::Microphone, &format!("{name} disconnected")),
            MicCue::ExtraBack(uid, name) => self.health.recovered(&format!("mic:{uid}"), Part::Microphone, &format!("{name} back")),
        };
        self.health_cue(cue);
    }

    /// Listening on: the live parts' health starts being watched.
    pub(crate) fn health_listening(&mut self) {
        self.listening = true;
    }

    /// Listening off: the listening parts' conditions no longer apply. The agent's, speech's and the screen's stay:
    /// those keep running (each switch turns off only itself), as do Heimdall's own (a relaunch
    /// problem) and the settings file's.
    pub(crate) fn health_off(&mut self) {
        self.listening = false;
        self.mics.reset();
        self.mic_audio.clear();
        self.worker_watch = None;
        for key in self.health.keys_with_prefix("mic:") {
            let cue = self.health.clear(&key);
            self.health_cue(cue);
        }
        for key in ["capture", "listen", "mic", "decisions", "permission:mic", "permission:system", "permission:accessibility"] {
            let cue = self.health.clear(key);
            self.health_cue(cue);
        }
    }

    /// The decision owner stopped answering (a panic, an exit or a missed deadline): only it is replaced, never
    /// Heimdall, so an agent answer in flight is never lost to it (bug 2026-10-26T15-49-44). Past the second
    /// failure in 10 minutes the replacement waits `pause_s` before loading; meanwhile commands and replies are
    /// paused and the agent is still reached by name. "Commands OK" comes with the replacement's first answer.
    pub(super) fn decider_owner_failed(&mut self, error: String, attempt: u32, pause_s: Option<u64>) {
        self.errors += 1;
        self.note_line(&format!("Decision owner failed; transcript retained: {error}"));
        self.component_status("decision", Some(match pause_s {
            Some(s) => format!("Decider paused for {s} s: {error}"),
            None => format!("Decider recovering: {error}"),
        }));
        let _ = self.index.evidence.event(
            "decider_owner_failed",
            "heimdall",
            &[],
            capture::host_now_ns(),
            serde_json::json!({"error":error,"attempt":attempt,"restarting":true,"pause_s":pause_s}),
        );
        let label = if pause_s.is_some() { "Commands paused · decider not answering" } else { "Commands paused · reloading" };
        let cue = self.health.dropped("decisions", Part::Commands, label);
        self.health_cue(cue);
    }

    /// The supervisor could not start a replacement owner (a thread could not spawn): commands stay off until
    /// the user restarts Heimdall himself.
    pub(super) fn decider_owner_stopped(&mut self, error: String) {
        self.errors += 1;
        self.note_line(&format!("Decision owner stopped; transcript retained: {error}"));
        self.component_status("decision", Some(format!("Decider stopped: {error}")));
        let _ = self.index.evidence.event("decider_owner_failed", "heimdall", &[], capture::host_now_ns(),
            serde_json::json!({"error":error,"restarting":false}));
        let cue = self.health.problem("decisions", Part::Commands, "Commands and replies are off", "Click to restart Heimdall", HealthControl::Relaunch);
        self.health_cue(cue);
    }

    pub(super) fn decider_loaded(&mut self, result: Result<(), (String, u32, u64)>) {
        match result {
            Ok(()) => {
                self.decider_phase(super::decider::Phase::Ready);
                self.component_status("decision", None);
                let cue = self
                    .health
                    .recovered("decisions", Part::Commands, "Commands OK");
                self.health_cue(cue);
            }
            Err((error, attempt, wait)) => {
                self.decider_phase(super::decider::Phase::Failed(error.clone()));
                let _ = self.index.evidence.event(
                    "decider_load_failed",
                    "heimdall",
                    &[],
                    capture::host_now_ns(),
                    serde_json::json!({"error":error,"attempt":attempt,"retry_s":wait}),
                );
                let cue = if attempt >= 3 {
                    self.health.problem(
                        "decisions",
                        Part::Commands,
                        "Commands and replies are off",
                        "Click to restart Heimdall",
                        HealthControl::Relaunch,
                    )
                } else {
                    self.health
                        .dropped("decisions", Part::Commands, "Commands paused · reloading")
                };
                self.health_cue(cue);
            }
        }
    }
    /// The archive's disk check, whenever its status changes: the free-space floor or an archive I/O error is a
    /// problem until it clears (recording resumes by itself once space is free).
    pub(crate) fn disk_health(&mut self, status: Option<&str>) {
        let cue = match status {
            None => self.health.recovered("disk", Part::Archive, "Recording again"),
            Some(s) if s.contains("GiB free") => self.health.problem("disk", Part::Archive, "Disk almost full · not recording", "Free some space; recording resumes by itself", HealthControl::ShowArchive),
            // Never the raw error on the island: it goes to the log and the evidence journal.
            Some(s) => {
                let error = s.trim_start_matches("Archive stopped: ");
                eprintln!("Archive stopped: {error}");
                let _ = self.index.evidence.event("archive_stopped", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"error": error}));
                let label = if archive_damaged(error) { "Archive database is damaged" } else { "Archive stopped · not recording" };
                self.health.problem("disk", Part::Archive, label, "Click to show it in Finder", HealthControl::ShowArchive)
            }
        };
        self.health_cue(cue);
    }

    /// Capture's archive pipeline stopped (a database or disk error): capture stopped with it, so nothing is
    /// recorded unarchived. Restart capture, which reopens the archive; again in 10 minutes climbs the ladder.
    /// A damaged store is not restarted: the archive is one store (docs/archive.md), so a restart or a relaunch
    /// reopens the same damaged file; the island says so instead and points at the archive.
    pub(crate) fn archive_stopped(&mut self, detail: &str) {
        if archive_damaged(detail) {
            eprintln!("Archive stopped: {detail}");
            let _ = self.index.evidence.event("archive_stopped", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"error": detail}));
            let cue = self.health.problem("disk", Part::Archive, "Archive database is damaged", "Click to show it in Finder", HealthControl::ShowArchive);
            self.health_cue(cue);
            return;
        }
        let rung = self.capture_ladder.climb(Instant::now());
        let cue = self.health.restarting("capture", Part::Archive, "Restarting recording…");
        self.health_cue(cue);
        self.request_recovery(match rung {
            Rung::Part => Recovery::RestartCapture(detail.into()),
            Rung::Pipeline => Recovery::RestartPipeline(detail.into()),
            Rung::Relaunch => Recovery::Relaunch { key: "capture".into(), part: Part::Archive, problem: "Recording keeps stopping".into() },
        });
    }

    /// Agent on was clicked: the harness is checked on a helper thread first. Meanwhile the agent stays in its dry
    /// run (nothing reaches the harness; `apply_menu_settings` holds it there while this is pending) and the tile
    /// shows the click. The live loop never waits for the check.
    pub fn start_preflight(&mut self) {
        self.preflight += 1;
        let (id, tx) = (self.preflight, self.internal.0.clone());
        let data = self.data.clone();
        self.agent_preflight_pending = Some(id);
        // Resolved here (a few file checks): the same resolver the harness launch uses.
        let executable = crate::bridge::fixture_executable();
        let spawned = std::thread::Builder::new().name("heimdall-preflight".into()).spawn(move || {
            let result = crate::preflight::check(&data, executable);
            let _ = tx.send(Internal::Preflight { id, result });
        });
        if let Err(e) = spawned {
            self.agent_preflight_pending = None;
            self.fail(&format!("Agent pre-flight could not start: {e}"));
        }
    }

    pub fn internal(&mut self, event: Internal) -> Result<()> {
        match event {
            Internal::Inference(completion)=>completion(self)?,
            Internal::Preflight { id, result } => {
                if self.agent_preflight_pending != Some(id) {
                    return Ok(());
                }
                self.agent_preflight_pending = None;
                let name = "Pi";
                self.index.evidence.event("agent_preflight", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"harness": name, "result": format!("{result:?}")}))?;
                match result {
                    Ok(_version) => {
                        let cue = self.health.recovered("agent", Part::Agent, &format!("{name} OK"));
                        self.health_cue(cue);
                        let mut next = self.settings.clone();
                        next.dry_run = self.tools.agent_enabled.stopped_for(StopReason::Agent);
                        self.apply_menu_settings(next)?;
                    }
                    Err(failure) => {
                        // Not accepted: Agent goes back off, as if the user had clicked it off.
                        self.tools.menu_intent(StopReason::Agent, false);
                        self.tools.menu_intent_processed(StopReason::Agent);
                        let (label, action, control) = failure.island();
                        self.note_line(&format!("Agent stays off: {label} ({failure:?})"));
                        let cue = self.health.problem("agent", Part::Agent, &label, &action, control);
                        self.health_cue(cue);
                    }
                }
                self.runtime_settings_ui();
                self.save_or_note();
            }

        }
        Ok(())
    }

    /// Agent off while a pre-flight runs: its answer is dropped.
    pub(crate) fn cancel_preflight(&mut self) {
        self.agent_preflight_pending = None;
    }

    pub fn preflight_pending(&self) -> bool {
        self.agent_preflight_pending.is_some()
    }
}

/// An archive error that means its SQLite database is damaged (rather than a disk or permission error).
pub(crate) fn archive_damaged(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    ["malformed", "not a database", "corrupt", "notadb"].iter().any(|w| e.contains(w))
}
