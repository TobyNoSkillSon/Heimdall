//! Agent health copy for the island and menu (de-duplicated, never raw transport errors).
use super::*;

/// Status copy is de-duplicated and never displays raw transport errors.
#[derive(Default)]
pub(super) struct AgentHealth {
    pub(super) health: crate::health::Health,
    pub(super) last: Option<(String, u32, bool, bool, bool)>,
    /// A restart of Pi is scheduled or under way (the supervisor's retry timer is set).
    pub(super) restarting: bool,
    /// A fault was shown and no turn has settled successfully since: "Agent OK" waits for one (E6.6, 18 Oct F4).
    pub(super) unproven: bool,
}
impl AgentHealth {
    pub(super) fn update(
        &mut self,
        status: &agent::Status,
        failures: u32,
        settings: &Settings,
        journal: &heimdall_archive::records::Writer,
        ui: ui::Ui,
    ) {
        for cue in self.record_cues(status, failures, settings, journal) {
            ui.send(ui::UiUpdate::Health(cue));
        }
    }
    pub(super) fn record_cues(&mut self, status: &agent::Status, failures: u32, settings: &Settings,
        journal: &heimdall_archive::records::Writer) -> Vec<ui::HealthCue> {
        let reason = format!("{status:?}");
        let key = (reason.clone(), failures, settings.dry_run, self.restarting, self.unproven);
        if self.last.as_ref() == Some(&key) { return vec![]; }
        let cues = self.cues(status, failures, settings);
        // Remembered with the state the cues left (a shown fault is unproven until a turn succeeds).
        self.last = Some((reason.clone(), failures, settings.dry_run, self.restarting, self.unproven));
        // Idle/responding changes already have agent transport receipts. Only faults,
        // held notices, or their recovery belong in this sparse health stream.
        if !cues.is_empty() || matches!(status, agent::Status::AuthBlocked(_) | agent::Status::NeedsReconciliation(_)) {
            crate::diagnostics::record(journal, "health", "agent:pi", json!({"component":"agent", "state":"transition", "reason":reason, "failures":failures, "off":settings.dry_run}));
        }
        for cue in &cues { crate::diagnostics::health(journal, cue, Some(&reason)); }
        cues
    }
    pub(super) fn cues(
        &mut self,
        status: &agent::Status,
        failures: u32,
        settings: &Settings,
    ) -> Vec<ui::HealthCue> {
        const KEY: &str = "harness";
        let part = ui::Part::Agent;
        if settings.dry_run {
            return self.health.clear(KEY).into_iter().collect();
        }
        let cues: Vec<ui::HealthCue> = match status {
            // Never a recovery claim without a successful turn since the fault (E6.6). An idle Pi that has not answered
            // yet (no turn can succeed until the user speaks, E1.16) ends the held restart or review cue without an "OK".
            agent::Status::Idle if self.unproven => self.health.clear(KEY).into_iter().collect(),
            agent::Status::Working if self.unproven => vec![],
            agent::Status::Idle | agent::Status::Working => self
                .health
                .recovered(KEY, part, "Agent OK")
                .into_iter()
                .collect(),
            agent::Status::ReloadPending => vec![],
            agent::Status::WaitingEngine | agent::Status::RateLimited { .. } => self
                .health
                .dropped(KEY, part, "Agent waiting for the engine")
                .into_iter()
                .collect(),
            agent::Status::WaitingProvider(reason) => self
                .health
                .dropped(KEY, part, reason)
                .into_iter()
                .collect(),
            // An interrupted reply is decided by the restart's session check (agent::Bridge::recover_receipts); the user
            // is asked only if recovery itself keeps failing, or when no restart is under way to decide it.
            agent::Status::NeedsReconciliation(_) if failures <= 3 && self.restarting => self
                .health
                .restarting(KEY, part, "Restarting the agent…")
                .into_iter()
                .collect(),
            agent::Status::NeedsReconciliation(_) => self
                .health
                .problem(
                    KEY,
                    part,
                    "Agent delivery needs review",
                    "Click to open the Agent window",
                    ui::HealthControl::OpenAgent,
                )
                .into_iter()
                .collect(),
            agent::Status::AuthBlocked(_) if failures <= 3 => self
                .health
                .restarting(KEY, part, "Restarting the agent…")
                .into_iter()
                .collect(),
            agent::Status::AuthBlocked(_) => self
                .health
                .problem(
                    KEY,
                    part,
                    "The agent could not recover",
                    "Check free disk space, then reload it in Open Agent",
                    ui::HealthControl::OpenAgent,
                )
                .into_iter()
                .collect(),
            agent::Status::Stopped => self.health.clear(KEY).into_iter().collect(),
        };
        // A failed turn or process (restart, review, could not recover) needs a successful turn to clear; waiting for
        // the engine or a key clears when the probe says it is back.
        if cues.iter().any(|c| matches!(c, ui::HealthCue::Problem { .. } | ui::HealthCue::Restarting { .. })) {
            self.unproven = true;
        }
        cues
    }
    /// A turn settled with `error:false`: a shown fault may now clear as recovered.
    pub(super) fn turn_succeeded(&mut self) {
        self.unproven = false;
    }
}
/// Lab instances only (`heimdall --lab-scenario`, lab/postinstall/): the real agent-health copy driven by scripted
/// statuses, so a scenario's crash and restart cues are the ones the app sends. A normal launch never builds one.
#[derive(Default)]
pub(crate) struct LabAgentHealth(AgentHealth);
impl LabAgentHealth {
    /// `status`: idle, working, crash (an interrupted reply under restart), stopped, or turn_ok (a turn settled
    /// with `error:false`).
    pub(crate) fn step(&mut self, status: &str, failures: u32, restarting: bool) -> Vec<ui::HealthCue> {
        let live = Settings { dry_run: false, ..Settings::default() };
        self.0.restarting = restarting;
        let status = match status {
            "idle" => agent::Status::Idle,
            "working" => agent::Status::Working,
            "crash" => agent::Status::NeedsReconciliation("lab-turn".into()),
            "stopped" => agent::Status::Stopped,
            "turn_ok" => {
                self.0.turn_succeeded();
                return vec![];
            }
            _ => return vec![],
        };
        self.0.cues(&status, failures, &live)
    }
}
pub(super) fn unavailable(
    _s: &Settings,
    data: &Path,
    session: &Path,
    error: String,
) -> Result<agent::Bridge<RecordedAdapter>> {
    let mut bridge = agent::Bridge::open(
        RecordedAdapter {
            inner: Adapter::Unavailable(Unavailable),
            journal: heimdall_archive::Journal::open(session)?,
            gate: None,
            ids: Default::default(),
            queries: HashMap::new(),
        },
        data.join("pi-spool.sqlite"),
    )?;
    bridge.harness.journal.event("agent_start_failed","agent:pi",&[],capture::host_now_ns(),json!({"error":error}))?;
    if !matches!(bridge.status, agent::Status::NeedsReconciliation(_)) {
        bridge.status = agent::Status::AuthBlocked(error);
    }
    Ok(bridge)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live() -> Settings {
        Settings { dry_run: false, ..Settings::default() }
    }

    /// Crash → restart → Idle: the held "Restarting the agent…" cue clears, and "Agent OK" waits for a turn that
    /// settles successfully (E6.6, E1.16: no turn can succeed until the user speaks).
    #[test]
    fn a_held_restart_cue_clears_when_the_new_pi_idles_without_an_ok() {
        let settings = live();
        let mut h = AgentHealth { restarting: true, ..Default::default() };
        let crash = h.cues(&agent::Status::NeedsReconciliation("turn-1".into()), 1, &settings);
        assert!(matches!(crash.as_slice(), [ui::HealthCue::Restarting { .. }]), "{crash:?}");
        assert!(h.unproven);
        h.restarting = false;
        assert_eq!(h.cues(&agent::Status::Idle, 0, &settings), [ui::HealthCue::Clear { key: "harness".into() }]);
        assert!(h.cues(&agent::Status::Idle, 0, &settings).is_empty(), "said once");
        assert!(h.cues(&agent::Status::Working, 0, &settings).is_empty(), "no recovery claim while unproven");
        h.turn_succeeded();
        assert!(h.cues(&agent::Status::Idle, 0, &settings).is_empty(), "nothing held, so nothing to call OK");
    }

    /// The same clear for the "Agent delivery needs review" problem, which shares the harness key.
    #[test]
    fn a_held_review_problem_clears_when_the_new_pi_idles_without_an_ok() {
        let settings = live();
        let mut h = AgentHealth::default();
        let shown = h.cues(&agent::Status::NeedsReconciliation("turn-1".into()), 4, &settings);
        assert!(matches!(shown.as_slice(), [ui::HealthCue::Problem { .. }]), "{shown:?}");
        assert_eq!(h.cues(&agent::Status::Idle, 0, &settings), [ui::HealthCue::Clear { key: "harness".into() }]);
        assert!(!h.health.holds("harness"), "the problem is no longer held");
    }

    /// A turn that succeeds while the cue is still held is the only thing that shows "Agent OK".
    #[test]
    fn agent_ok_follows_a_settled_successful_turn_only() {
        let settings = live();
        let mut h = AgentHealth { restarting: true, ..Default::default() };
        h.cues(&agent::Status::NeedsReconciliation("turn-1".into()), 1, &settings);
        h.restarting = false;
        assert!(h.cues(&agent::Status::Working, 0, &settings).is_empty());
        h.turn_succeeded();
        assert_eq!(
            h.cues(&agent::Status::Idle, 0, &settings),
            [ui::HealthCue::Recovered { key: "harness".into(), part: ui::Part::Agent, label: "Agent OK".into() }]
        );
    }
}
