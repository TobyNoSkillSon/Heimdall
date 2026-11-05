//! Setup's Getting ready rows after the models (docs/setup.md, "Getting ready"; 5 Nov: Start waits until
//! everything is green): the voice, listening, the decider, This Mac's model when it is the agent, and the agent, each
//! green, on its way, or red with why and its fix. The models' own rows are `model_store`'s. The same rows, models
//! included, are kept in `<data>/setup/status.json` for `Heimdall setup --check` (setup_cli.rs): one set of checks.
//!
//! The agent's row is green only after one real request to the agent's model answered, through the same route the
//! agent uses (the provider's credential and endpoint, the relay through T3, or This Mac's engine).
use super::*;
use crate::background_model::BackgroundModel;

/// The background call's job name for the agent check: let through while Agent is still off (Setup turns it on at
/// Start), and logged as such.
pub(crate) const AGENT_CHECK_JOB: &str = "setup agent check";

/// The agent check for the agent as it is now (`key`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum AgentCheck {
    #[default]
    None,
    Running(String),
    Passed(String),
    Failed(String, String),
}

/// The agent to check: (what identifies it, its label, the model to ask). `None`: no agent chosen.
pub(crate) fn agent_target(s: &crate::settings::Settings, relay: &(Vec<ui::RelayModel>, Vec<ui::RelayAccount>)) -> Option<Result<(String, String, BackgroundModel), String>> {
    if s.agent_relay {
        let account = s.agent_relay_account.clone().or_else(|| relay.1.first().map(|a| a.id.clone()));
        let model = s.agent_relay_model.clone().or_else(|| relay.0.first().map(|m| m.id.clone()));
        let (Some(account), Some(model)) = (account, model) else { return Some(Err("T3 Code isn\u{2019}t connected yet".into())) };
        let label = relay.0.iter().find(|m| m.id == model).map_or(model.clone(), |m| m.name.clone());
        return Some(Ok((format!("relay:{account}:{model}"), format!("{label} \u{b7} Claude Code"), BackgroundModel { provider: format!("{}{account}", ui::T3_PROVIDER_PREFIX), id: model })));
    }
    match &s.agent_model {
        agent::provider::AgentModel::Unchosen => None,
        agent::provider::AgentModel::Local => Some(Ok((format!("local:{}", s.engine_model), "This Mac".into(), BackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: s.engine_model.clone() }))),
        agent::provider::AgentModel::Api { provider, id } => {
            let name = s.api_models.iter().find(|m| m.key() == (provider.as_str(), id.as_str())).map_or(id.clone(), |m| m.name.clone());
            let via = agent::provider::provider(provider).map_or(provider.clone(), |p| p.name.to_string());
            Some(Ok((format!("api:{provider}:{id}"), format!("{name} \u{b7} {via}"), BackgroundModel { provider: provider.clone(), id: id.clone() })))
        }
    }
}

/// A failed check's fix: a sign-in or key the provider refused goes back to the provider step; anything else retries.
pub(crate) fn agent_fix(why: &str) -> ui::ReadyFix {
    let w = why.to_lowercase();
    if ["401", "403", "sign in", "signed in", "key", "unauthor", "credential", "no subscription"].iter().any(|k| w.contains(k)) {
        ui::ReadyFix::Step { title: "Sign In\u{2026}".into(), step: ui::SetupStep::Providers }
    } else {
        ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(ui::UiAction::CheckAgent) }
    }
}

/// "17.4 GB": a download's size, as Setup's rows say it.
pub(crate) fn gb(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1e9)
}

fn row(key: &str, title: &str, detail: Option<String>, symbol: &str, state: ui::ReadyState) -> ui::ReadyRow {
    ui::ReadyRow { key: key.into(), title: title.into(), detail, symbol: symbol.into(), state }
}

fn working(words: &str) -> ui::ReadyState {
    ui::ReadyState::Working { words: words.into(), progress: None }
}

/// Waiting on another row: no spinner (nothing of its own moves).
fn blocked(words: &str) -> ui::ReadyState {
    ui::ReadyState::Blocked { words: words.into() }
}

fn problem(why: &str, fix: Option<ui::ReadyFix>) -> ui::ReadyState {
    ui::ReadyState::Problem { why: why.into(), fix }
}

/// The agent's row from the check and the target.
pub(crate) fn agent_row(target: Option<Result<(String, String, BackgroundModel), String>>, check: &AgentCheck, engine_waits: bool) -> ui::ReadyRow {
    let choose = || Some(ui::ReadyFix::Step { title: "Choose\u{2026}".into(), step: ui::SetupStep::Providers });
    let (detail, state) = match target {
        None => (None, problem("No agent chosen yet", choose())),
        Some(Err(why)) => (None, problem(&why, Some(ui::ReadyFix::Step { title: "Connect\u{2026}".into(), step: ui::SetupStep::T3 }))),
        Some(Ok((key, label, _))) => (Some(label), match check {
            _ if engine_waits => blocked("Waits for This Mac\u{2019}s model"),
            AgentCheck::Passed(k) if *k == key => ui::ReadyState::Ready,
            AgentCheck::Failed(k, why) if *k == key => problem(&crate::model_store::short(why), Some(agent_fix(why))),
            _ => working("Asking it a first question\u{2026}"),
        }),
    };
    row("agent", "Agent", detail, "sparkles", state)
}

/// The voice's row: Phonon S speaking the chosen preset, learnt (its profile, so Heimdall knows its own voice).
pub(crate) fn voice_row(speaking: bool, preset: Option<&str>, learnt: bool, learning: bool, waits: bool) -> ui::ReadyRow {
    let detail = preset.map(|p| format!("Phonon S \u{b7} {p}"));
    let state = if learnt { ui::ReadyState::Ready }
        else if !speaking { problem("Speaking is off", Some(ui::ReadyFix::Action { title: "Turn On".into(), action: Box::new(ui::UiAction::SetSpeakReplies(true)) })) }
        else if learning { working("Learning the voice\u{2026}") }
        else if waits { problem("Couldn\u{2019}t learn the voice yet", Some(ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(ui::UiAction::RetryVoice) })) }
        else { working("Loading\u{2026}") };
    row("voice", "Voice", detail, "speaker.wave.2.fill", state)
}

/// Listening's row: Parakeet loaded and listening on, and sound arriving (`mic_ready`, the Microphone row green). Without
/// it the row waits for the microphone, which says why and offers its fix (VM run 5 Nov 05:07: Listening green beside
/// a red "No microphone found").
pub(crate) fn listening_row(models_ready: bool, allowed: bool, paused: bool, worker_ready: bool, mic_ready: bool) -> ui::ReadyRow {
    let state = if worker_ready && mic_ready { ui::ReadyState::Ready }
        else if worker_ready { blocked("Waits for the microphone") }
        else if !allowed { problem("Allow Heimdall first", Some(ui::ReadyFix::Step { title: "Allow\u{2026}".into(), step: ui::SetupStep::Permissions })) }
        else if paused { problem("Listening is off", Some(ui::ReadyFix::Action { title: "Turn On".into(), action: Box::new(ui::UiAction::SetListening(true)) })) }
        else if !models_ready { blocked("Waits for its models") }
        else { working("Loading Parakeet\u{2026}") };
    row("listening", "Listening", Some("Parakeet".into()), "waveform", state)
}

/// Whether sound arrives from the input chosen for listening: the capturing primary microphone (`capturing`, its uid
/// when its audio is recent) and, when one is chosen and connected, only once it is that one (review 5 Nov: a mic
/// picked while capture was starting left the row green on the old input).
pub(crate) fn hearing_chosen(chosen: Option<&str>, capturing: Option<&str>) -> bool {
    capturing.is_some() && chosen.is_none_or(|c| Some(c) == capturing)
}

/// A capture failure in a few plain words for the row (the log keeps the whole error).
pub(crate) fn capture_words(why: &str) -> String {
    let lower = why.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") { return "the audio system did not answer".into(); }
    if lower.contains("permission") || lower.contains("not authorized") || lower.contains("denied") { return "macOS did not allow it".into(); }
    let line = why.lines().next().unwrap_or(why);
    if line.chars().count() > 40 { format!("{}\u{2026}", line.chars().take(39).collect::<String>()) } else { line.to_string() }
}

/// The capture's state for the Microphone row: a failure reported (`Pipeline::fail`'s "capture" status) wins; then a
/// start in flight; listening on with neither is a capture that never began.
pub(crate) fn capture_start(listening_on: bool, starting: bool, failure: Option<String>) -> CaptureStart {
    match (failure, starting) {
        (Some(why), false) => CaptureStart::Failed(why.trim_start_matches("Capture: ").to_string()),
        (_, true) => CaptureStart::Starting,
        (None, false) if listening_on => CaptureStart::Failed("the microphone did not open".into()),
        _ => CaptureStart::NotYet,
    }
}

/// How long the chosen input may stay silent once capture runs before its row turns red ("No sound from …"), rather
/// than waiting with a spinner forever.
pub(crate) const MIC_SILENT: std::time::Duration = std::time::Duration::from_secs(10);

/// The microphone's row (5 Nov: the microphone is required): green once audio actually arrives from the
/// input in use. No input at all, or one silent past `MIC_SILENT`, is red with Choose… (Setup's Microphone step).
/// Where the microphone's capture is when no sound arrives yet (`microphone_row`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CaptureStart {
    /// Listening is not on yet: nothing to open.
    NotYet,
    /// Listening is on and the capture is starting.
    Starting,
    /// Listening is on but no capture runs or starts (it failed, or never began): why.
    Failed(String),
}

pub(crate) fn microphone_row(inputs: usize, name: Option<String>, hearing: bool, capturing: Option<std::time::Duration>, start: CaptureStart) -> ui::ReadyRow {
    let choose = || Some(ui::ReadyFix::Step { title: "Choose\u{2026}".into(), step: ui::SetupStep::Microphone });
    // VM run 5 Nov 07:16: a capture that never started left this row "Waits for listening" beside Listening's "Waits
    // for the microphone", forever. Once listening is on, the row says what the capture does: starting, or why not.
    let retry = || Some(ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(ui::UiAction::RetrySystemAudio) });
    let state = if inputs == 0 { problem("No microphone found", choose()) }
        else if hearing { ui::ReadyState::Ready }
        else { match capturing {
            None => match start {
                CaptureStart::NotYet => blocked("Waits for listening"),
                CaptureStart::Starting => working("Opening the microphone\u{2026}"),
                CaptureStart::Failed(why) => problem(&format!("Couldn't start listening: {}", capture_words(&why)), retry()),
            },
            Some(d) if d < MIC_SILENT => working("Listening for sound\u{2026}"),
            Some(_) => problem(&format!("No sound from {}", name.as_deref().unwrap_or("the microphone")), choose()),
        } };
    row("microphone", "Microphone", name, "mic.fill", state)
}

/// The decider's row: its download (from model_store), then its load and first answer.
pub(crate) fn decider_row(phase: &decider::Phase, label: String, download: Option<&crate::model_store::Row>) -> ui::ReadyRow {
    if let Some(d) = download.filter(|d| d.state != crate::model_store::State::Done) {
        let state = match &d.state {
            crate::model_store::State::Failed(why) => problem(&crate::model_store::short(why), Some(ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(ui::UiAction::RetryModelDownloads) })),
            crate::model_store::State::Downloading => ui::ReadyState::Working { words: "Downloading".into(), progress: Some((d.done, d.total)) },
            _ => blocked(&format!("{} to download", gb(d.total))),
        };
        return row("decider", "Decider", Some(label), "target", state);
    }
    let state = match phase {
        decider::Phase::Ready => ui::ReadyState::Ready,
        decider::Phase::Loading => working("Loading\u{2026}"),
        decider::Phase::Off => blocked("Starts with listening"),
        // It retries by itself.
        decider::Phase::Failed(why) => ui::ReadyState::Working { words: format!("{} \u{b7} retrying", crate::model_store::short(why)), progress: None },
    };
    row("decider", "Decider", Some(label), "target", state)
}

/// System audio's row, when there is something to say (optional; listening runs on the microphone
/// alone): its ask out or failed (`capture::tap_gate`), never holding Start. Before the ask, and once its taps run, none.
pub(crate) fn system_audio_row(access: Option<capture::tap_gate::TapAccess>) -> Option<ui::ReadyRow> {
    use capture::tap_gate::TapAccess;
    let why = match access? {
        TapAccess::Open => return None,
        TapAccess::Waiting => "Waits for its permission".to_string(),
        TapAccess::Off(_) => "Off: not allowed".to_string(),
    };
    Some(row("system-audio", "System audio", None, "waveform.circle", ui::ReadyState::Optional { why }))
}

/// The free memory memory search's worker needs to load (`embedding/worker.py`'s admission).
pub(crate) const MEMORY_NEEDED: u64 = 8 << 30;

/// Memory search's row, once its model is downloaded (until then its download row stands): green once the worker
/// loaded and embedded; otherwise not needed to start (`Optional`), with why: too little free memory (VM run 5 Nov
/// 05:43: a 16 GB Mac never ran it, yet its row was green), or nothing indexed yet.
pub(crate) fn memory_row(downloaded: bool, search: crate::embeddings::Search, free: Option<u64>) -> Option<ui::ReadyRow> {
    use crate::embeddings::Search;
    if !downloaded { return None; }
    let state = match search {
        Search::Running => ui::ReadyState::Ready,
        Search::NoMemory => ui::ReadyState::Optional { why: "Needs 8 GB free memory".into() },
        Search::NotYet if free.is_some_and(|f| f < MEMORY_NEEDED) => ui::ReadyState::Optional { why: "Needs 8 GB free memory".into() },
        Search::NotYet => ui::ReadyState::Optional { why: "Starts with the first thing to index".into() },
    };
    Some(row("memory", "Memory search", Some("EmbeddingGemma 2".into()), "magnifyingglass", state))
}

/// This Mac's model's row, while the local engine is the agent.
pub(crate) fn engine_row(model: Option<&ui::EngineModel>, state: &ui::EngineState) -> ui::ReadyRow {
    let Some(m) = model else { return row("engine", "This Mac", None, "laptopcomputer", problem("No model for the engine", None)) };
    let size = m.chosen().and_then(|q| q.size_bytes);
    let download = ui::UiAction::DownloadEngineModel { id: m.id.clone(), quant: m.quant.clone(), then_select: true };
    let s = match (&m.download, m.ready(), state) {
        (Some(d), _, _) if d.error.is_some() => problem(&crate::model_store::short(d.error.as_deref().unwrap_or_default()), Some(ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(download) })),
        (Some(d), _, _) => ui::ReadyState::Working { words: "Downloading".into(), progress: Some((d.done_bytes, d.total_bytes.or(size).unwrap_or(0))) },
        (None, false, _) => problem(&format!("{} to download", size.map(gb).unwrap_or_else(|| "Its weights are".into())), Some(ui::ReadyFix::Action { title: "Download".into(), action: Box::new(download) })),
        (None, true, ui::EngineState::Ready | ui::EngineState::Idle) => ui::ReadyState::Ready,
        (None, true, ui::EngineState::Error(why)) => problem(&crate::model_store::short(why), Some(ui::ReadyFix::Action { title: "Retry".into(), action: Box::new(ui::UiAction::SetEngine(true)) })),
        (None, true, _) => working("Loading\u{2026}"),
    };
    row("engine", "This Mac", Some(m.label.clone()), "laptopcomputer", s)
}

impl Pipeline {
    /// The rows now (the models' are `model_store`'s).
    pub(crate) fn setup_readiness(&self) -> ui::SetupReadiness {
        let mut rows = vec![];
        let route = self.tools.speech_voices.route();
        let key = self.heimdall_voice.key(&self.tools, &route);
        let learnt = self.heimdall_voice.enrolled(&key, &heimdall_voice::recipe_for(&route));
        let learning = self.heimdall_voice.enrolling.as_deref() == Some(key.as_str());
        let waits = self.heimdall_voice.retry.contains_key(&key);
        let preset = match &route { crate::speech_voice::Route::Phonon(p) => Some(p.clone()), _ => None };
        // The microphone first: listening needs it.
        let mics = &self.menu_lists.0;
        let inputs: Vec<&ui::Microphone> = mics.devices.iter().filter(|d| d.connected).collect();
        // The input actually capturing (the primary microphone's open source, its audio recent) against the one chosen.
        let capturing = self.mic_audio.iter().filter(|(id, t)| t.elapsed() < std::time::Duration::from_secs(5) && self.primary_mic(id))
            .find_map(|(id, _)| match &self.sources.get(id)?.kind { capture::SourceKind::InputDevice { uid, name } => Some((uid.clone(), name.clone())), _ => None });
        let chosen = self.settings.microphone.as_ref().map(|m| m.uid.clone()).filter(|u| inputs.iter().any(|d| &d.id == u));
        let hearing = hearing_chosen(chosen.as_deref(), capturing.as_ref().map(|(uid, _)| uid.as_str()));
        let shown = if hearing { capturing.as_ref().map(|(uid, _)| uid.clone()) } else { chosen.clone().or_else(|| mics.in_use.clone()) };
        let name = shown.and_then(|id| inputs.iter().find(|d| d.id == id).map(|d| d.name.clone()).or_else(|| capturing.as_ref().map(|(_, n)| n.clone())));
        let listening_on = self.live_worker.as_ref().is_some_and(|w| w.is_ready() && !w.is_finished());
        let start = capture_start(listening_on, self.capture_starting.is_some(), self.component_status.get("capture").cloned());
        let mic = microphone_row(inputs.len(), name, hearing, self.capture_since.map(|t| t.elapsed()), start);
        let mic_ready = mic.state == ui::ReadyState::Ready;
        rows.push(mic);
        rows.push(voice_row(self.settings.speak_replies, preset.as_deref(), learnt, learning, waits));
        let worker = self.live_worker.as_ref().is_some_and(|w| w.is_ready() && !w.is_finished());
        rows.push(listening_row(self.listen_models_ready(), self.settings.setup_requested, self.settings.paused, worker, mic_ready));
        let download = crate::model_store::global().and_then(|d| d.rows().into_iter().find(|r| r.id == crate::model_store::decider_group(self.settings.decider_precision)));
        rows.push(decider_row(&self.decider_phase, self.decider_pick().label(), download.as_ref()));
        let memory_in = crate::model_store::global().is_some_and(|d| d.rows().iter().any(|r| r.id == crate::model_store::EMBEDDING && r.state == crate::model_store::State::Done));
        let search = self._embeddings.as_ref().map_or(crate::embeddings::Search::NotYet, |j| j.search());
        let free = (memory_in && search == crate::embeddings::Search::NotYet).then(crate::diagnostics::available_memory_bytes).flatten();
        rows.extend(memory_row(memory_in, search, free));
        rows.extend(system_audio_row(capture::system_audio()));
        let local = self.settings.agent_model.is_local() && !self.settings.agent_relay;
        let mut engine_waits = false;
        if local {
            let (model, state) = self.engine_for_setup();
            let r = engine_row(model.as_ref(), &state);
            engine_waits = r.state != ui::ReadyState::Ready;
            rows.push(r);
        }
        rows.push(agent_row(agent_target(&self.settings, &self.tools.relay_catalog.lists()), &self.agent_check, engine_waits));
        ui::SetupReadiness { rows }
    }

    /// The settings tick: the agent checked when it changed (or became reachable), the rows sent when they changed,
    /// and the status file kept current.
    pub(crate) fn setup_ready_tick(&mut self) {
        // The public edition's Setup only (review 5 Nov: in both editions it checked the agent on every launch).
        if !crate::edition::PUBLIC { return; }
        let unfinished = crate::setup_wizard::setup_unfinished(&self.settings);
        // Claude Code chosen by `Heimdall setup` while T3 Code installed: connected once it is here.
        if self.t3_connect_after_install && crate::setup_wizard::current_t3(ui::T3Install::Idle).ready() {
            self.t3_connect_after_install = false;
            crate::setup_cli::send(&self.tools, ui::UiAction::ConnectT3);
        }
        // The chosen voice is learnt from Phonon S, which must be in memory; after a relaunch nothing else loads it.
        // Once per launch (a load that fails is not tried every tick; the voice row's Retry tries again).
        if unfinished && self.settings.speak_replies && !self.phonon_tried && !self.tools.speech_voices.phonon_loaded() {
            self.phonon_tried = true;
            let voices = self.tools.speech_voices.clone();
            let back = self.internal.0.clone();
            let _ = std::thread::Builder::new().name("heimdall-phonon-setup".into()).spawn(move || {
                if let Err(e) = voices.load_phonon() { crate::diagnostics::line("setup", &format!("{e:#}")); }
                let _ = back.send(super::resilience::Internal::Inference(Box::new(|p: &mut Pipeline| { p.heimdall_voice_tick(); Ok(()) })));
            });
        }
        let target = agent_target(&self.settings, &self.tools.relay_catalog.lists());
        let local_waits = self.settings.agent_model.is_local() && !self.settings.agent_relay && {
            let (model, state) = self.engine_for_setup();
            engine_row(model.as_ref(), &state).state != ui::ReadyState::Ready
        };
        if let Some(Ok((key, _, model))) = &target {
            let fresh = !matches!(&self.agent_check, AgentCheck::Running(k) | AgentCheck::Passed(k) | AgentCheck::Failed(k, _) if k == key);
            // Only while the first Setup is not done: afterwards the agent is the agent's business, not Setup's.
            if fresh && !local_waits && unfinished { self.start_agent_check(key.clone(), model.clone()); }
        }
        let readiness = self.setup_readiness();
        let changed = self.readiness_seen.as_ref() != Some(&readiness);
        if changed {
            self.ui(ui::UiUpdate::SetupReadiness(readiness.clone()));
            self.readiness_seen = Some(readiness.clone());
        }
        // The status file: on every change, and as a heartbeat (`Heimdall setup` reads an old one as Heimdall not
        // running).
        if changed || self.status_written.is_none_or(|t| t.elapsed() >= crate::setup_cli::HEARTBEAT) {
            self.status_written = Some(Instant::now());
            self.write_setup_status(&readiness);
        }
    }

    /// One real request to the agent's model, off the live loop; its answer reaches `agent_check`.
    pub(crate) fn start_agent_check(&mut self, key: String, model: BackgroundModel) {
        self.agent_check = AgentCheck::Running(key.clone());
        if self.replay || cfg!(test) { return; }
        let back = self.internal.0.clone();
        let _ = std::thread::Builder::new().name("heimdall-agent-check".into()).spawn(move || {
            let e = crate::background_model::endpoint_for(model);
            let c = crate::background_call::Call {
                job: AGENT_CHECK_JOB,
                model: e.model.clone().unwrap_or_default(),
                system: "You answer with the JSON the schema asks for, nothing else.",
                schema: serde_json::json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}),
                effort: "low",
                prompt: "Set ok to true.".into(),
                history: Vec::new(),
                session: None,
            };
            let result = if e.key.is_none() && ui::t3_instance(e.provider).is_none() {
                Err(format!("{} isn\u{2019}t signed in", if e.provider.is_empty() { "The provider" } else { e.provider }))
            } else {
                crate::background_call::ask_blocking(&e, &c).map(|_| ()).map_err(|err| format!("{err:#}"))
            };
            crate::diagnostics::line("setup", &format!("agent check {key}: {}", result.as_ref().map_or_else(|e| format!("failed: {}", e.chars().take(200).collect::<String>()), |_| "answered".into())));
            let _ = back.send(super::resilience::Internal::Inference(Box::new(move |p: &mut Pipeline| {
                p.agent_check = match result { Ok(()) => AgentCheck::Passed(key), Err(why) => AgentCheck::Failed(key, why) };
                p.setup_ready_tick();
                Ok(())
            })));
        });
    }

    /// Getting ready ▸ the agent's Retry: the check again now.
    pub(crate) fn recheck_agent(&mut self) {
        self.agent_check = AgentCheck::None;
        self.setup_ready_tick();
    }

    /// `Heimdall setup`'s requests: each choice as the Setup window's actions, on the app's action queue.
    pub(crate) fn setup_requests_tick(&mut self) {
        if self.arsenal_after_install && crate::setup_wizard::find_arsenal(&t3::install::app_dirs(&heimdall_home::Env::current().user_home)).is_some() {
            self.arsenal_after_install = false;
            crate::setup_cli::send(&self.tools, ui::UiAction::SetArsenal(true));
        }
        let commands = crate::setup_cli::take_requests(&self.data);
        if commands.is_empty() { return; }
        let t3 = crate::setup_wizard::current_t3(ui::T3Install::Idle);
        let (engine, _) = self.engine_for_setup();
        let home = heimdall_home::Env::current().user_home;
        let ctx = crate::setup_cli::Context {
            t3_ready: t3.ready(),
            t3_connected: t3.connect_allowed,
            engine_download: engine.filter(|m| !m.ready() && m.download.is_none()).map(|m| (m.id, m.quant)),
            arsenal_found: crate::setup_wizard::find_arsenal(&t3::install::app_dirs(&home)).is_some(),
        };
        for c in commands {
            crate::diagnostics::line("setup", &format!("from Heimdall setup: {c:?}"));
            match &c {
                crate::setup_cli::Command::CustomConnected { provider } => {
                    if let Some(p) = agent::provider::provider(provider) { self.tools.api.connected_elsewhere(p); }
                }
                crate::setup_cli::Command::Wizard => self.ui(ui::UiUpdate::ShowSetup),
                crate::setup_cli::Command::Arsenal if !ctx.arsenal_found => self.arsenal_after_install = true,
                crate::setup_cli::Command::Provider { provider } if provider == "claude-code" && !ctx.t3_ready => self.t3_connect_after_install = true,
                crate::setup_cli::Command::T3 { connect: true } if !ctx.t3_ready => self.t3_connect_after_install = true,
                // Start only from a Getting ready that is all green, here and now (never a stale status file's word).
                crate::setup_cli::Command::Finish => {
                    let models = crate::model_store::global().map(|d| crate::model_store::view(&d)).unwrap_or_default();
                    if !(models.finished() && self.setup_readiness().ready()) {
                        crate::diagnostics::line("setup", "Heimdall setup --finish refused: not every Getting ready row is green");
                        continue;
                    }
                }
                _ => {}
            }
            for action in crate::setup_cli::actions_for(&c, &ctx) {
                crate::setup_cli::send(&self.tools, action);
            }
        }
    }

    /// `<data>/setup/status.json`: the models' rows and these, for `Heimdall setup --check`.
    fn write_setup_status(&self, readiness: &ui::SetupReadiness) {
        let models = crate::model_store::global().map(|d| crate::model_store::view(&d)).unwrap_or_default();
        let status = crate::setup_cli::Status::of(&models, readiness, !crate::setup_wizard::setup_unfinished(&self.settings));
        if let Err(e) = status.save(&self.data) { crate::diagnostics::line("setup", &format!("status not written: {e:#}")); }
    }
}

#[cfg(test)]
#[path = "setup_ready_tests.rs"]
mod tests;
