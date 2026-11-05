//! The Decider section's state (`ui::DeciderStatus`) and the decider hot-swap on the live loop (`decider_swap` is the
//! owner-thread half). A precision change (the menu, Setup, the settings file or the `settings` tool) or a version
//! change (`decider_model`, v6 or v5: the settings file or the `settings` tool) comes through `apply_menu_settings`;
//! this starts the swap and answers its outcome.
use super::*;
use decide::precision::{self, Pick, Precision};

/// What the decider is doing, as the menu shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Not loaded (Listening off, or before the first decision).
    #[default]
    Off,
    Loading,
    Ready,
    /// A load failed and nothing runs (it retries by itself).
    Failed(String),
}

impl Pipeline {
    /// The decider in settings: its version (`decider_model`) in its precision (`decider_precision`).
    pub(crate) fn decider_pick(&self) -> Pick {
        Pick::new(self.settings.decider_model, self.settings.decider_precision)
    }
    fn set_decider_pick(&mut self, p: Pick) {
        self.settings.decider_model = p.model;
        self.settings.decider_precision = p.precision;
    }

    /// The menu's and Setup's picture of the decider.
    pub(crate) fn decider_status(&self) -> ui::DeciderStatus {
        let models = self.models.join("decide");
        let chosen = self.decider_pick();
        let running = self.send_worker.as_ref().and_then(|w| w.choice.running());
        let state = match (&self.decider_swapping, &self.decider_phase) {
            (Some((_, to)), _) => ui::DeciderState::Swapping { to: to.precision.key().into() },
            (None, Phase::Off) => ui::DeciderState::Off,
            (None, Phase::Loading) => ui::DeciderState::Loading,
            (None, Phase::Ready) => ui::DeciderState::Ready,
            (None, Phase::Failed(why)) => ui::DeciderState::Failed(why.clone()),
        };
        // A missing folder: what would load for the choice (the nearest present one), said in the menu. The header names
        // the version that runs (V5 when V6's folders are missing), its precisions that version's folders.
        let resolved = precision::resolve(chosen, |p| precision::present(&models, p));
        let shown = running.map_or(resolved.used.model, |p| p.model);
        // The public edition downloads a missing precision when it is chosen; its size is the download's.
        let downloads = crate::model_store::global();
        let disk = |p: Precision| downloads.as_ref().and_then(|d| d.lock().group(&crate::model_store::decider_group(p)).map(|g| g.bytes())).unwrap_or(Pick::new(shown, p).disk_bytes());
        ui::DeciderStatus {
            version: precision::version_label(Pick::new(shown, chosen.precision)),
            options: Precision::ALL.iter().map(|p| ui::DeciderOption {
                key: p.key().into(),
                label: p.label().into(),
                disk_bytes: disk(*p),
                memory_bytes: p.memory_bytes(),
                present: precision::present(&models, Pick::new(shown, *p)),
            }).collect(),
            chosen: chosen.precision.key().into(),
            running: running.map(|p| p.precision.key().into()),
            fallback: (resolved.precision_fell_back() && self.decider_swapping.is_none()).then(|| resolved.used.precision.key().into()),
            reason: self.decider_reason.clone().or_else(|| resolved.model_fell_back().then(|| resolved.reason()).flatten()),
            state,
            downloadable: downloads.is_some(),
        }
    }

    pub(crate) fn publish_decider(&self) {
        self.ui(ui::UiUpdate::Decider(self.decider_status()));
    }

    /// The decider loaded, failed to, or started loading (`resilience::decider_loaded`, `prepare_decisions`).
    pub(super) fn decider_phase(&mut self, phase: Phase) {
        if self.decider_phase == phase { return; }
        self.decider_phase = phase;
        self.publish_decider();
    }

    /// Listening off unloaded it. A swap in flight went with the owner before its new decider answered: the setting
    /// goes back to the one that ran, so an untested decider never becomes the next load's by default.
    pub(super) fn decider_unloaded(&mut self) {
        if let Some((from, to)) = self.decider_swapping.take() {
            if self.decider_pick() == to {
                self.set_decider_pick(from);
                self.save_or_note();
            }
        }
        self.decider_phase = Phase::Off;
        self.publish_decider();
    }

    /// The setting changed from `from`: the running decider swaps to it. With no owner yet the next load reads it; while
    /// a swap runs, its completion starts the next one.
    pub(super) fn decider_pick_changed(&mut self, from: Pick) {
        let to = self.decider_pick();
        self.decider_reason = None;
        if self.decider_swapping.is_some() || from == to {
            self.publish_decider();
            return;
        }
        let Some(owner) = self.send_worker.as_ref() else {
            self.publish_decider();
            return;
        };
        if owner.choice.chosen() == to {
            self.publish_decider();
            return;
        }
        // Not on this Mac yet (the public edition downloads it): what runs is already the nearest one, so nothing
        // reloads; the owner's next load reads the new choice, and the download's landing swaps it in.
        let models = self.models.join("decide");
        if !precision::present(&models, to) && owner.choice.running() == Some(precision::resolve(to, |p| precision::present(&models, p)).used) {
            owner.choice.set_chosen(to);
            self.publish_decider();
            return;
        }
        let _ = self.index.evidence.event("decider_swap", "heimdall", &[], capture::host_now_ns(),
            serde_json::json!({"from":from.precision.key(),"to":to.precision.key(),"from_model":from.model.key(),"to_model":to.model.key(),"from_folder":from.name(),"to_folder":to.name()}));
        match owner.swap(from, to, move |p, out| { p.decider_swapped(from, to, out); Ok(()) }) {
            Ok(()) => self.decider_swapping = Some((from, to)),
            Err(e) => {
                // The owner refused it (its queue is full or it stopped): the setting goes back, the menu says why.
                self.set_decider_pick(from);
                self.decider_reason = Some(format!("{} didn\u{2019}t load: {e:#}", to.label()));
                self.save_or_note();
            }
        }
        self.publish_decider();
    }

    /// The chosen precision finished downloading while another one runs in its place: it swaps in, as a pick does.
    pub(super) fn decider_download_landed(&mut self) {
        let to = self.decider_pick();
        let running = self.send_worker.as_ref().and_then(|w| w.choice.running());
        let Some(from) = running.filter(|r| *r != to && self.decider_swapping.is_none()) else {
            self.publish_decider();
            return;
        };
        let Some(owner) = self.send_worker.as_ref() else { return };
        let _ = self.index.evidence.event("decider_swap", "heimdall", &[], capture::host_now_ns(),
            serde_json::json!({"from":from.precision.key(),"to":to.precision.key(),"from_model":from.model.key(),"to_model":to.model.key(),"from_folder":from.name(),"to_folder":to.name(),"reason":"downloaded"}));
        if owner.swap(from, to, move |p, out| { p.decider_swapped(from, to, out); Ok(()) }).is_ok() {
            self.decider_swapping = Some((from, to));
        }
        self.publish_decider();
    }

    /// The owner's answer to a swap (`decider_swap::Outcome`).
    pub(super) fn decider_swapped(&mut self, from: Pick, to: Pick, out: decider_swap::Outcome) {
        self.decider_swapping = None;
        let _ = self.index.evidence.event("decider_swapped", "heimdall", &[], capture::host_now_ns(), match &out {
            decider_swap::Outcome::Swapped { used } => serde_json::json!({"to":to.precision.key(),"to_model":to.model.key(),"used":used.precision.key(),"used_folder":used.name(),"outcome":"swapped"}),
            decider_swap::Outcome::RolledBack { reason, used } => serde_json::json!({"to":to.precision.key(),"to_model":to.model.key(),"back_to":used.precision.key(),"back_to_folder":used.name(),"outcome":"rolled_back","reason":reason}),
            decider_swap::Outcome::Failed { reason } => serde_json::json!({"to":to.precision.key(),"to_model":to.model.key(),"outcome":"failed","reason":reason}),
        });
        match out {
            decider_swap::Outcome::Swapped { used } => {
                self.decider_reason = None;
                self.decider_phase = Phase::Ready;
                self.decider_loaded(Ok(()));
                // Another pick arrived while this one ran (a settings-file edit): it goes next.
                if self.decider_pick() != to {
                    self.decider_pick_changed(to);
                    return;
                }
                // The chosen one was missing and a fallback loaded, but it finished downloading meanwhile (its landing
                // found this swap in flight): it swaps in now.
                if swap_again(used, to, |p| precision::present(&self.models.join("decide"), p)) {
                    self.decider_download_landed();
                    return;
                }
            }
            decider_swap::Outcome::RolledBack { reason, .. } => {
                self.roll_back_precision(from, to);
                self.decider_reason = Some(reason);
                self.decider_phase = Phase::Ready;
                self.decider_loaded(Ok(()));
                if self.decider_pick() != from {
                    // A newer pick arrived meanwhile (the file, the settings tool): it goes next.
                    self.decider_pick_changed(from);
                    return;
                }
            }
            decider_swap::Outcome::Failed { reason } => {
                self.roll_back_precision(from, to);
                self.decider_reason = Some(reason.clone());
                // Nothing runs: commands are paused as for any failed load, and the owner loads `from` on its next request.
                self.decider_loaded(Err((reason, 1, 0)));
            }
        }
        self.publish_decider();
    }

    /// The setting back to `from` after a failed swap to `to`, unless a newer pick replaced `to` meanwhile.
    fn roll_back_precision(&mut self, from: Pick, to: Pick) {
        if self.decider_pick() == to {
            self.set_decider_pick(from);
            self.save_or_note();
        }
    }
}

/// After a swap to `to` loaded `used`: whether `to` should load now (a fallback ran, and `to` is here by now).
pub(super) fn swap_again(used: Pick, to: Pick, present: impl Fn(Pick) -> bool) -> bool {
    used != to && present(to)
}

#[cfg(test)]
mod swap_again_tests {
    use super::*;

    /// Review 5 Nov: a decider that lands while another swap is in flight swaps in once that swap ends.
    #[test]
    fn a_decider_that_landed_during_a_swap_loads_after_it() {
        let (v6_4, v6_8) = (Pick::new(decide::precision::DeciderModel::V6, Precision::Int4), Pick::new(decide::precision::DeciderModel::V6, Precision::Int8));
        assert!(swap_again(v6_4, v6_8, |p| p == v6_8), "8-bit landed while the 4-bit fallback loaded");
        assert!(!swap_again(v6_4, v6_8, |_| false), "still downloading: the landing swaps it in later");
        assert!(!swap_again(v6_8, v6_8, |_| true), "it loaded itself");
    }
}
