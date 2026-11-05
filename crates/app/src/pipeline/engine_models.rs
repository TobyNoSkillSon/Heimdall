//! The menu's Engine section (30 Oct): every precision of every catalog model, whether it is on this Mac, downloading
//! one (only on the user's click), deleting one, and which one the engine loads. The engine manager
//! (`inference_engine::Manager`) only runs Splash; this side decides what it may run: nothing that is not on disk,
//! so neither startup nor any switch downloads by itself (28 Oct, the user: "dont download them just add in setting").
//! The engine and the agent's model are independent: loading a model never touches the agent's choice, and an engine
//! on with no model loaded waits for the user's pick (`ui::EngineState::Waiting`) instead of loading one by itself.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use crossbeam_channel::{Receiver, Sender};

use super::Pipeline;
use crate::inference_engine::models::{self, Download, DownloadEvent, Installer, Roots};
use crate::inference_engine::Config;

/// What the manager was last asked to run: on, model id, precision.
type Sent = (bool, String, String);

struct Pending {
    download: Download,
    then_select: bool,
    /// The agent's choice when the download began; a pick of another model meanwhile cancels `then_select`.
    picked: Picked,
    done: u64,
    total: Option<u64>,
}
type Picked = (agent::provider::AgentModel, bool, String, Option<String>);

pub(crate) struct EngineModels {
    pub(crate) roots: Roots,
    pub(crate) installer: Installer,
    /// The sources installed, as last looked.
    installed: BTreeSet<String>,
    pending: Option<Pending>,
    /// Per model id: the last download that failed, or a refused delete, shown on its row until the next try.
    errors: BTreeMap<String, ui::EngineDownload>,
    /// A confirmed precision not saved yet (its weights missing, or over the memory ceiling): the SelectEngineModel
    /// that follows it on Confirm downloads it (or refuses it); the saved precision changes only when it lands.
    picked_quant: Option<(String, String)>,
    events_tx: Sender<DownloadEvent>,
    events: Receiver<DownloadEvent>,
    /// The manager's last engine update, before the rows are filled in.
    last: Mutex<Option<ui::UiUpdate>>,
    sent: Option<Sent>,
}

impl EngineModels {
    pub(crate) fn new(data: &std::path::Path) -> Self {
        let (events_tx, events) = crossbeam_channel::unbounded();
        let installer = if cfg!(test) {
            Installer::Disabled
        } else {
            // The same `heimdall-splash` the engine manager starts (beside Heimdall's own binary, or the override's).
            Installer::Program(Config::effective(data).0.executable())
        };
        let mut this = Self {
            roots: Roots::for_data(data),
            installer,
            installed: BTreeSet::new(),
            pending: None,
            errors: BTreeMap::new(),
            picked_quant: None,
            events_tx,
            events,
            last: Mutex::new(None),
            sent: None,
        };
        this.refresh(&Config::effective(data).0);
        this
    }
    /// Look at the disk again; whether anything changed.
    fn refresh(&mut self, config: &Config) -> bool {
        let now: BTreeSet<String> = config
            .models
            .iter()
            .flat_map(|m| m.quants.iter())
            .filter(|q| models::installed(&self.roots.models, &q.source))
            .map(|q| q.source.clone())
            .collect();
        let changed = now != self.installed;
        self.installed = now;
        changed
    }
    fn is_installed(&self, source: &str) -> bool {
        self.installed.contains(source)
    }
}

impl Pipeline {
    fn engine_config(&self) -> Config {
        Config::effective(&self.data).0
    }
    /// The precision the settings choose for a model (its default when none or an unknown one is chosen).
    pub(crate) fn engine_chosen(&self, config: &Config, id: &str) -> Option<crate::inference_engine::Quant> {
        config.model(id).ok().map(|m| m.chosen(self.settings.engine_quant_choice(id)).clone())
    }
    /// Setup's This Mac row (setup_ready.rs): the engine's model as the menu shows it (its download, whether its weights
    /// are here), and the engine's state now.
    pub(crate) fn engine_for_setup(&self) -> (Option<ui::EngineModel>, ui::EngineState) {
        let config = self.engine_config();
        let em = &self.engine_models;
        let id = if self.settings.engine_model.is_empty() { config.default_model.clone() } else { self.settings.engine_model.clone() };
        let model = config.models.iter().find(|m| m.id == id).map(|m| {
            let download = match &em.pending {
                Some(p) if p.download.id == m.id => Some(ui::EngineDownload { quant: p.download.quant.clone(), done_bytes: p.done, total_bytes: p.total, error: None }),
                _ => em.errors.get(&m.id).cloned(),
            };
            config.ui_model(m, self.settings.engine_quant_choice(&m.id), &|s| em.is_installed(s), download)
        });
        let state = match em.last.lock().unwrap().as_ref() { Some(ui::UiUpdate::Engine { state, .. }) => state.clone(), _ => ui::EngineState::Off };
        (model, state)
    }
    /// The engine's model at its chosen precision is on this Mac.
    pub(crate) fn engine_ready(&self, config: &Config) -> bool {
        self.engine_chosen(config, &self.settings.engine_model).is_some_and(|q| self.engine_models.is_installed(&q.source))
    }
    /// What the manager knows while nothing is picked: off, on the catalog's default (a valid model, never run).
    fn engine_idle(config: &Config) -> Sent {
        let quant = config.model(&config.default_model).map(|m| m.quant.clone()).unwrap_or_default();
        (false, config.default_model.clone(), quant)
    }
    /// What the manager starts with at launch: on only when the picked weights are here (never a download at startup).
    pub(crate) fn engine_startup(&mut self) -> Sent {
        let config = self.engine_config();
        self.engine_models.refresh(&config);
        let sent = match self.engine_chosen(&config, &self.settings.engine_model) {
            Some(q) => (self.settings.engine_on && self.engine_ready(&config), self.settings.engine_model.clone(), q.quant),
            None => Self::engine_idle(&config),
        };
        self.engine_models.sent = Some(sent.clone());
        sent
    }
    /// Bring the manager to the settings: the chosen precision when it is on disk. A precision still downloading
    /// leaves the engine on the one it serves; another precision or model is a restart (off, then on).
    pub(crate) fn sync_engine(&mut self, explicit: bool) {
        let config = self.engine_config();
        let model = self.settings.engine_model.clone();
        let ready = self.engine_ready(&config);
        let sent = self.engine_models.sent.clone();
        let want: Sent = if let Some(quant) = self.engine_chosen(&config, &model).map(|q| q.quant).filter(|_| !model.is_empty()) {
            if self.settings.engine_on && !ready {
                if sent.as_ref().is_some_and(|s| s.0 && s.1 == model) {
                    // Keep serving the precision it has until the new one is here.
                    self.engine_publish();
                    return;
                }
                (false, model, quant)
            } else {
                (self.settings.engine_on, model, quant)
            }
        } else {
            // No model picked: nothing runs (an engine serving one stops; the manager keeps a valid model, off).
            match sent.as_ref().filter(|s| !s.1.is_empty() && config.model(&s.1).is_ok()) {
                Some(s) => (false, s.1.clone(), s.2.clone()),
                None => Self::engine_idle(&config),
            }
        };
        if !explicit && sent.as_ref() == Some(&want) {
            self.engine_publish();
            return;
        }
        if let Some(manager) = &self.inference_engine {
            if let Some(s) = sent.as_ref().filter(|s| s.0 && want.0 && (s.1 != want.1 || s.2 != want.2)) {
                manager.set(false, s.1.clone(), s.2.clone());
            }
            manager.set(want.0, want.1.clone(), want.2.clone());
        }
        self.engine_models.sent = Some(want);
        self.engine_publish();
    }
    /// The manager's engine update, kept and sent with the rows filled in.
    pub(crate) fn engine_receive(&self, update: ui::UiUpdate) {
        *self.engine_models.last.lock().unwrap() = Some(update);
        self.engine_publish();
    }
    /// The last engine update again, with every model's precisions, sizes, memory, disk state and download, and the
    /// engine's memory ceiling. Settings on with no model it may load (and nothing running) reads as waiting for a
    /// pick, so nothing downloads or loads by itself.
    pub(crate) fn engine_publish(&self) {
        let config = self.engine_config();
        let base = self.engine_models.last.lock().unwrap().clone();
        let base = base.unwrap_or_else(|| config.update(&self.settings.engine_model, ui::EngineState::Off, None, None));
        let ui::UiUpdate::Engine { mut state, mut model_id, mut model_label, port, context_limit, cache_used_bytes, cache_limit_bytes, .. } = base else { return };
        let em = &self.engine_models;
        let rows = config
            .models
            .iter()
            .map(|m| {
                let download = match &em.pending {
                    Some(p) if p.download.id == m.id => Some(ui::EngineDownload { quant: p.download.quant.clone(), done_bytes: p.done, total_bytes: p.total, error: None }),
                    _ => em.errors.get(&m.id).cloned(),
                };
                config.ui_model(m, self.settings.engine_quant_choice(&m.id), &|s| em.is_installed(s), download)
            })
            .collect();
        // Asked to run a model the manager has not taken up yet (its last update names another, or is Off): that model,
        // starting, so a pick never flickers back to the one before (review 30 Oct).
        if let Some(s) = em.sent.as_ref().filter(|s| s.0) {
            if model_id != s.1 || state == ui::EngineState::Off {
                state = ui::EngineState::Starting;
                model_id = s.1.clone();
                model_label = config.model(&s.1).map(|m| m.label.clone()).unwrap_or_default();
            }
        }
        // On with nothing it may load (no pick, or the pick's weights gone): waiting for the user's pick, never a fault and
        // never a load or download by itself (30 Oct).
        if self.settings.engine_on && state == ui::EngineState::Off && !self.engine_ready(&config) {
            state = ui::EngineState::Waiting;
        }
        // Off or waiting, the model named is the pick (none: empty), not whatever the manager last held.
        if matches!(state, ui::EngineState::Off | ui::EngineState::Waiting) {
            let picked = self.engine_ready(&config).then(|| self.settings.engine_model.clone()).unwrap_or_default();
            model_label = config.model(&picked).map(|m| m.label.clone()).unwrap_or_default();
            model_id = picked;
        }
        self.ui(ui::UiUpdate::Engine {
            state,
            model_id,
            model_label,
            models: rows,
            port,
            context_limit,
            cache_used_bytes,
            cache_limit_bytes,
            memory_limit_bytes: config.memory_limit_bytes(),
        });
    }
    /// The menu opened (or a download or delete ended): look at the disk again.
    pub(crate) fn engine_refresh(&mut self) {
        let config = self.engine_config();
        if self.engine_models.refresh(&config) {
            self.sync_engine(false);
        }
    }
    fn agent_picked(&self, id: &str) -> Picked {
        (self.settings.agent_model.clone(), self.settings.agent_relay, self.settings.engine_model.clone(), self.settings.engine_quant.get(id).cloned())
    }
    /// The model at this precision loaded and the agent on it, the engine on: only for the user's own words
    /// (`UiAction::SelectEngineModel`: the island's "Switch to the local model?" Yes, the `settings` tool).
    fn engine_select(&mut self, id: &str, quant: &str) -> anyhow::Result<()> {
        let mut next = self.settings.clone();
        next.engine_quant.insert(id.into(), quant.into());
        next.engine_model = id.into();
        next.agent_model = agent::provider::AgentModel::Local;
        next.engine_on = true;
        next.engine_added = true;
        // Any other model row ends the Claude relay (docs/claude-relay.md).
        next.agent_relay = false;
        self.apply_menu_settings(next)
    }
    /// A refused pick or download, on the model's row. Its `total_bytes` is always `Some` (the size, else 0); a
    /// refused or failed delete (`engine_refuse_delete`) has `None`, so the menu tells them apart.
    fn engine_refuse(&mut self, id: &str, quant: &str, why: &str) {
        eprintln!("Engine models: {id} {quant}: {why}");
        let total = self.engine_config().quant(id, quant).ok().and_then(|q| q.size_bytes).or(Some(0));
        self.engine_models.errors.insert(id.into(), ui::EngineDownload { quant: quant.into(), done_bytes: 0, total_bytes: total, error: Some(why.into()) });
        self.engine_publish();
    }
    fn engine_refuse_delete(&mut self, id: &str, quant: &str, why: &str) {
        eprintln!("Engine models: delete {id} {quant}: {why}");
        self.engine_models.errors.insert(id.into(), ui::EngineDownload { quant: quant.into(), done_bytes: 0, total_bytes: None, error: Some(why.into()) });
        self.engine_publish();
    }
    fn engine_label(&self, id: &str) -> String {
        self.engine_config().model(id).map(|m| m.label.clone()).unwrap_or_else(|_| id.into())
    }
    /// Why a precision cannot run: it needs more memory than the engine's ceiling (derived `ram_bytes`).
    fn over_ceiling(config: &Config, q: &crate::inference_engine::Quant) -> Option<String> {
        let (ram, limit) = (q.ram_bytes?, config.memory_limit_bytes()?);
        let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
        (ram > limit).then(|| format!("Needs {:.0} GB of memory; the engine's limit is {:.0} GB", gb(ram), gb(limit)))
    }
    /// The menu opened: old reasons go, the disk is looked at again.
    pub(crate) fn engine_menu_opened(&mut self) {
        self.engine_models.errors.clear();
        self.engine_refresh();
        self.engine_publish();
    }

    /// Agent ▸ Model ▸ a precision, confirmed: saved when its weights are here and it fits the memory ceiling (the
    /// engine moves to it when it serves this model). Otherwise nothing is saved yet: the SelectEngineModel that
    /// follows downloads it, or says why not. Unknown models or precisions change nothing.
    pub(crate) fn set_engine_quant(&mut self, id: String, quant: String) -> anyhow::Result<()> {
        let config = self.engine_config();
        let Ok(q) = config.quant(&id, &quant).cloned() else {
            eprintln!("Engine models: {id} offers no precision {quant}; nothing changed");
            return Ok(());
        };
        if !self.engine_models.is_installed(&q.source) || Self::over_ceiling(&config, &q).is_some() {
            self.engine_models.picked_quant = Some((id, quant));
            return Ok(());
        }
        self.engine_models.picked_quant = None;
        if self.settings.engine_quant.get(&id) == Some(&quant) {
            return Ok(());
        }
        let mut next = self.settings.clone();
        next.engine_quant.insert(id, quant);
        self.apply_menu_settings(next)
    }
    /// The Engine section's quick-pick: the engine on, loading this precision (one on this Mac and under the memory
    /// ceiling; anything else is refused with the reason on its row). The agent's model stays as it is.
    pub(crate) fn load_engine_model(&mut self, id: String, quant: String) -> anyhow::Result<()> {
        let config = self.engine_config();
        let Ok(q) = config.quant(&id, &quant).cloned() else {
            eprintln!("Engine models: {id} offers no precision {quant}; nothing loads");
            return Ok(());
        };
        if let Some(why) = Self::over_ceiling(&config, &q) {
            self.engine_refuse(&id, &quant, &why);
            return Ok(());
        }
        self.engine_models.refresh(&config);
        if !self.engine_models.is_installed(&q.source) {
            self.engine_refuse(&id, &quant, "Not on this Mac: download it first");
            return Ok(());
        }
        let mut next = self.settings.clone();
        next.engine_quant.insert(id.clone(), quant);
        next.engine_model = id;
        next.engine_on = true;
        next.engine_added = true;
        let unchanged = next == self.settings;
        self.apply_menu_settings(next)?;
        // A click is explicit: a failed engine on the same model starts again (a change already reached the manager).
        if unchanged { self.sync_engine(true); }
        Ok(())
    }
    /// The agent on the engine's model at its chosen precision (`UiAction::SelectEngineModel`, the user's own words): the
    /// agent's model when that precision is here; otherwise it downloads first and is selected when it lands.
    pub(crate) fn select_engine_model(&mut self, id: String) -> anyhow::Result<()> {
        let config = self.engine_config();
        let model = config.model(&id)?;
        let picked = self.engine_models.picked_quant.take().filter(|(m, _)| *m == id).and_then(|(_, q)| model.quant(&q).cloned());
        let q = picked.unwrap_or_else(|| self.engine_chosen(&config, &id).expect("a known model"));
        // Over the ceiling: never downloaded or switched by a pick (its download button still downloads it).
        if let Some(why) = Self::over_ceiling(&config, &q) {
            self.engine_refuse(&id, &q.quant, &why);
            return Ok(());
        }
        if self.engine_models.is_installed(&q.source) {
            return self.engine_select(&id, &q.quant);
        }
        self.download_engine_model(id, q.quant, true)
    }
    /// A precision's download button (or a pick whose weights are missing): `heimdall-splash pull`, one at a time.
    pub(crate) fn download_engine_model(&mut self, id: String, quant: String, then_select: bool) -> anyhow::Result<()> {
        let config = self.engine_config();
        let Ok(q) = config.quant(&id, &quant).cloned() else {
            eprintln!("Engine models: {id} offers no precision {quant}; nothing downloads");
            return Ok(());
        };
        // A precision over the ceiling downloads on request but never becomes the agent's model by itself.
        let then_select = then_select && Self::over_ceiling(&config, &q).is_none();
        self.engine_refresh();
        if self.engine_models.is_installed(&q.source) {
            return if then_select { self.engine_select(&id, &quant) } else { Ok(()) };
        }
        if let Some(p) = &mut self.engine_models.pending {
            if p.download.source == q.source {
                p.then_select |= then_select;
                return Ok(());
            }
            let (other, other_quant) = (p.download.id.clone(), p.download.quant.clone());
            let busy = format!("Another download is running ({} {other_quant})", self.engine_label(&other));
            self.engine_refuse(&id, &quant, &busy);
            return Ok(());
        }
        let em = &self.engine_models;
        match Download::start(&em.installer, &em.roots, &id, &quant, &q.source, q.size_bytes, em.events_tx.clone()) {
            Ok(download) => {
                eprintln!("Engine models: downloading {} ({:?} bytes)", q.source, q.size_bytes);
                let picked = self.agent_picked(&id);
                self.engine_models.errors.remove(&id);
                self.engine_models.pending = Some(Pending { download, then_select, picked, done: 0, total: q.size_bytes });
                self.engine_publish();
            }
            Err(e) => self.engine_refuse(&id, &quant, &format!("{e:#}")),
        }
        Ok(())
    }
    pub(crate) fn cancel_engine_download(&mut self, id: &str) {
        if let Some(p) = self.engine_models.pending.as_ref().filter(|p| p.download.id == id) {
            p.download.cancel();
        }
        self.engine_models.errors.remove(id);
        self.engine_publish();
    }
    /// A precision's Delete, confirmed in the menu: refused while the engine serves it or it downloads; the reason
    /// shows on its row (`EngineModel::download.error`).
    pub(crate) fn delete_engine_model(&mut self, id: String, quant: String) -> anyhow::Result<()> {
        let config = self.engine_config();
        let Ok(q) = config.quant(&id, &quant).cloned() else {
            eprintln!("Engine models: {id} offers no precision {quant}; nothing deleted");
            return Ok(());
        };
        let serving = self.engine_models.sent.as_ref().is_some_and(|s| s.0 && s.1 == id && s.2 == quant);
        if serving {
            self.engine_refuse_delete(&id, &quant, "The engine is using it: turn it off or pick another first");
            return Ok(());
        }
        // Any pull in flight: Splash takes the installation lock only when it publishes (upstream.rs `publish`), so a
        // delete now could take files that pull is about to link.
        if let Some(p) = &self.engine_models.pending {
            let why = if p.download.source == q.source { "It is downloading: cancel the download first".to_string() }
                else { format!("A download is running ({} {}): wait for it or cancel it first", self.engine_label(&p.download.id), p.download.quant) };
            self.engine_refuse_delete(&id, &quant, &why);
            return Ok(());
        }
        match models::delete(&self.engine_models.roots, &q.source) {
            Ok(deleted) => {
                eprintln!("Engine models: deleted {} ({} bytes freed, {} paths)", q.source, deleted.bytes, deleted.paths.len());
                crate::diagnostics::record(&self.evidence, "engine_model_deleted", "heimdall",
                    serde_json::json!({"model": id, "quant": quant, "source": q.source, "bytes_freed": deleted.bytes}));
                self.engine_models.errors.remove(&id);
            }
            Err(e) => self.engine_refuse_delete(&id, &quant, &format!("Could not delete: {e:#}")),
        }
        let config = self.engine_config();
        self.engine_models.refresh(&config);
        // The picked precision gone: no model is picked, so its weights coming back later never load by themselves.
        let picked = self.settings.engine_model == id && self.engine_chosen(&config, &id).is_some_and(|c| c.quant == quant);
        if picked && !self.engine_models.is_installed(&q.source) {
            let mut next = self.settings.clone();
            next.engine_model.clear();
            self.apply_menu_settings(next)?;
            self.save_or_note();
        }
        self.sync_engine(false);
        Ok(())
    }
    /// The download events, for the live loop to wait on (at most four progress updates a second).
    pub(crate) fn engine_download_receiver(&self) -> Receiver<DownloadEvent> {
        self.engine_models.events.clone()
    }
    /// Download progress and endings not yet handled (also polled with the engine's own events).
    pub(crate) fn engine_download_events(&mut self) {
        let events: Vec<_> = self.engine_models.events.try_iter().collect();
        for event in events {
            self.engine_download_event(event);
        }
    }
    pub(crate) fn engine_download_event(&mut self, event: DownloadEvent) {
        {
            match event {
                DownloadEvent::Progress { id, quant, done, total } => {
                    if let Some(p) = self.engine_models.pending.as_mut().filter(|p| p.download.id == id && p.download.quant == quant) {
                        p.done = done;
                        p.total = total.or(p.total);
                    }
                    self.engine_publish();
                }
                DownloadEvent::Done { id, quant, result, canceled } => {
                    let Some(p) = self.engine_models.pending.take_if(|p| p.download.id == id && p.download.quant == quant) else { return };
                    let (then_select, picked, done, total) = (p.then_select, p.picked.clone(), p.done, p.total);
                    drop(p);
                    match result {
                        Ok(()) => {
                            eprintln!("Engine models: {id} {quant} downloaded");
                            self.engine_models.errors.remove(&id);
                        }
                        Err(_) if canceled => {}
                        Err(e) => {
                            eprintln!("Engine models: the download of {id} {quant} failed: {e}");
                            let size = config_size(&self.engine_config(), &id, &quant);
                            self.engine_models.errors.insert(id.clone(), ui::EngineDownload { quant: quant.clone(), done_bytes: done, total_bytes: total.or(size).or(Some(0)), error: Some(e) });
                        }
                    }
                    let config = self.engine_config();
                    self.engine_models.refresh(&config);
                    let landed = config.quant(&id, &quant).is_ok_and(|q| self.engine_models.is_installed(&q.source));
                    if landed && then_select && self.agent_picked(&id) == picked {
                        match self.engine_select(&id, &quant) {
                            Ok(()) => self.save_or_note(),
                            Err(e) => eprintln!("Engine models: could not select {id} {quant}: {e:#}"),
                        }
                    }
                    self.sync_engine(false);
                }
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn engine_downloading(&self) -> Option<(String, String, bool)> {
        self.engine_models.pending.as_ref().map(|p| (p.download.id.clone(), p.download.quant.clone(), p.then_select))
    }
    #[cfg(test)]
    pub(crate) fn engine_sent(&self) -> Option<(bool, String, String)> {
        self.engine_models.sent.clone()
    }
    #[cfg(test)]
    pub(crate) fn engine_models_mut(&mut self) -> &mut EngineModels {
        &mut self.engine_models
    }
}

fn config_size(config: &Config, id: &str, quant: &str) -> Option<u64> {
    config.quant(id, quant).ok().and_then(|q| q.size_bytes)
}
