//! Settings, the local inference engine and the menu's view of them: apply, save, retry, engine control, UI values.
use super::*;

impl Pipeline {
    /// Only the live application starts ownership; replay/idle diagnostics and tests do not.
    pub fn start_inference_engine(&mut self) {
        if !self.replay && self.inference_engine.is_none() {
            self.tools.engine_signal.store(1, Ordering::Release);
            // On only with the chosen precision on disk: Splash would download a missing one at its start.
            let (on, model, quant) = self.engine_startup();
            self.inference_engine = Some(crate::inference_engine::Manager::start(self.data.clone(), on, model, quant, self.session.clone()));
        }
    }
    #[cfg(test)]
    pub(crate) fn attach_test_engine(&mut self,manager:crate::inference_engine::Manager) {
        self.inference_engine=Some(manager);
    }
    pub fn request_engine_stop(&self) {
        if let Some(manager) = &self.inference_engine { manager.request_shutdown(); }
    }
    pub(crate) fn inference_engine_events(&mut self) {
        let updates: Vec<_> = self.inference_engine.as_ref().map(|m| m.events.try_iter().collect()).unwrap_or_default();
        for update in updates {
            if let ui::UiUpdate::Engine { state, .. } = &update {
                self.tools.engine_signal.store(if matches!(state,ui::EngineState::Ready|ui::EngineState::Idle) {2} else {1}, Ordering::Release);
                self.tools.engine_loading.store(matches!(state, ui::EngineState::Starting | ui::EngineState::Loading { .. }), Ordering::Release);
            }
            match update {
                ui::UiUpdate::Health(cue) => self.health_cue(Some(cue)),
                engine @ ui::UiUpdate::Engine { .. } => self.engine_receive(engine),
                other => self.ui(other),
            }
        }
        self.engine_download_events();
    }
    pub fn initial_ui(&self) {
        // API Providers: the added models at once; key checks and the catalog follow from the network.
        self.tools.api.set_settings(&self.settings);
        #[cfg(not(test))]
        if !self.replay { self.tools.api.refresh(true); }

        {
            let config = crate::inference_engine::Config::effective(&self.data).0;
            let starting = self.settings.engine_on && self.engine_ready(&config);
            self.engine_receive(config.update(&self.settings.engine_model, if starting { ui::EngineState::Starting } else { ui::EngineState::Off }, None, None));
        }
        self.ui(ui::UiUpdate::BugNotes(self.bugs.listed.clone()));
        self.ui(ui::UiUpdate::Settings(self.ui_settings_value()));
        self.ui(ui::UiUpdate::Identity(self.settings.identity.clone()));
        self.publish_decider();
        self.ui(ui::UiUpdate::Agent(ui::AgentState {
            harness: ui::Harness::Pi,
            session: ui::SessionState::Stopped,
        }));
        self.ui(ui::UiUpdate::Listen(ui::ListenState::NeedsSetup));
        self.send_speakers();
        self.runtime_settings_ui();
        // The voices (Phonon S's 25 presets) from the start: Setup's Voice step comes before the agent ever starts,
        // whose supervisor used to be the first to publish them (VM run 5 Nov: the grid was empty until a relaunch).
        self.tools.speech_voices.attach_ui(self.ui);
        self.ui(ui::UiUpdate::SpeechVoice(self.tools.speech_voices.ui_state()));
        crate::setup_wizard::load_consent(&self.settings);
        match crate::setup_wizard::first_launch(crate::edition::PUBLIC, self.settings.setup_requested, crate::setup_wizard::resume_step(&self.settings), || heimdall_home::current().needs_choice()) {
            Some(ui::SetupStep::Welcome) => self.ui(ui::UiUpdate::ShowSetup),
            Some(step) => self.ui(ui::UiUpdate::ShowSetupAt(step)),
            None => {}
        }
    }
    pub fn runtime_settings_ui(&self) {
        self.ui(ui::UiUpdate::RuntimeSettings(self.runtime_settings_value()));
    }
    /// What the menu shows for the app-owned settings (tiles, Recording, Reload).
    pub fn runtime_settings_value(&self) -> ui::RuntimeSettings {
        ui::RuntimeSettings {
            dry_run: self.menu_agent_off(),
            archive_format: match self.settings.archive_format {
                settings::ArchiveFormat::NativeLossless => 0,
                settings::ArchiveFormat::Flac24 => 1,
                settings::ArchiveFormat::Flac16kMono => 2,
            },
            archive_choice_pending: self.settings.archive_choice_pending,
            retention_days: self.settings.retention_days,
            archive_status: self.disk_status.lock().unwrap().clone(),
            screen: self.settings.screen.ui(),
            seeing: !self.settings.screen_off,
            pending_reload: self.pending_reload.keys().cloned().collect(),
            setup_requested: self.settings.setup_requested,
            setup_done: !crate::setup_wizard::setup_unfinished(&self.settings),
        }
    }
    /// The menu's picture of the agent's choice again (the providers' selection and the settings), after a pick that
    /// was refused: the radio goes back to what runs.
    pub fn publish_settings(&self) {
        self.tools.api.publish();
        self.ui(ui::UiUpdate::Settings(self.ui_settings_value()));
    }
    /// The menu's Speaking and Launch at Login state.
    pub fn ui_settings_value(&self) -> ui::Settings {
        ui::Settings { speak_replies: self.settings.speak_replies, launch_at_login: self.settings.launch_at_login, voice_speed_percent: self.settings.voice_speed_percent,
            engine_thinking: self.settings.engine_thinking_level().into(), wake_sound: self.settings.wake_sound,
            engine_context: self.settings.engine_context.get(&self.settings.engine_model).copied(), background_model: self.settings.background_model.ui(),
            background_efforts: self.settings.background_efforts.iter().map(|e| e.ui()).collect(),
            task_models: self.settings.task_models.iter().filter_map(|m| m.ui()).collect(),
            background_off: crate::background_model::off_tasks(&self.settings.background_off),
            islands_shown: !self.settings.islands_hidden,
            islands_on_streams: self.settings.islands_on_streams, agent_relay: self.settings.agent_relay,
            agent_relay_model: self.settings.agent_relay_model.clone(), agent_relay_account: self.settings.agent_relay_account.clone(), engine_added: self.settings.engine_added,
            relay_added: self.settings.relay_added || self.settings.agent_relay, collapsed: self.settings.collapsed_sections() }
    }
    /// Seeing off (or the screen stream stopping): no frame stays in memory or can be attached later.
    pub fn clear_screen_frames(&mut self) {
        // The placeholder has no store: building one here held the live loop 2 s (26 Oct 23:01).
        self.screen_buffer = Arc::new(Mutex::new(screen::ScreenBuffer::idle()));
        // Held command frames go too: no decision after Seeing turned off attaches a frame captured before.
        self.screen_holds.clear();
    }
    /// Seeing on/off from the menu. The live loop owns the screen stream and starts or stops it.
    pub fn set_seeing(&mut self, on: bool) -> Result<()> {
        let mut next = self.settings.clone();
        next.screen_off = !on;
        self.apply_menu_settings(next)?;
        // A failed save is noted and retried; the live loop must still start or stop the stream.
        self.save_or_note();
        Ok(())
    }
    /// The Agent menu item reads the latch, never in-memory or file settings.
    pub fn menu_agent_off(&self) -> bool { self.tools.agent_enabled.stopped_for(agent::delivery::StopReason::Agent) }
    #[cfg(test)]
    pub(crate) fn test_persist_settings(&mut self) { self.replay = false; }
    #[cfg(test)]
    pub(crate) fn test_agent(&self) -> &AgentRuntime { self.agent.as_ref().unwrap() }
    pub(crate) fn lab_night_running(&self) -> bool { self.night.as_ref().is_some_and(|n| n.running()) }
    pub(crate) fn install_lab_night(&mut self, night: crate::night_runtime::Scheduler) { self.night = Some(night); }
    pub fn night_admission(&self, permit: bool) {
        if let Some(night) = &self.night { night.allow(permit); }
    }
}
/// A failure repeated within this window is counted, not noted again (`Pipeline::fail`).
pub(crate) const REPEAT_QUIET: std::time::Duration = std::time::Duration::from_secs(30);
impl Pipeline {
    pub fn listening_status(&self) {
        self.ui(ui::UiUpdate::Listen(ui::ListenState::Listening));
        for (component, line) in &self.menu_problems {
            self.ui(ui::UiUpdate::ComponentStatus { component: component.clone(), message: Some(line.clone()) });
        }
    }
    /// A failure. A part that could not start or keeps stopping is held under its own key until it runs again
    /// (each clears where that part comes back). A one-off error (one question, one write, one event) is journaled
    /// and noted, never held: held under "pipeline" it read "Listening interrupted" until relaunch (23 Oct).
    pub fn fail(&mut self, text: &str) {
        use super::menu_problem::{DECISIONS_STOPPED, SEEING_STOPPED, TRANSCRIPTION_STOPPED};
        self.failure = Some(text.into());
        // The same failure again within `REPEAT_QUIET` is counted, not noted again (its stop still applies below): on
        // 31 Oct 22:21-22:22 "Listen failed: evidence worker failed" was logged, journaled and shown 1,000+ times in 90 s.
        let repeat = match self.repeated_failure.as_mut() {
            Some((last, since, repeats)) if last == text && since.elapsed() < REPEAT_QUIET => {
                *repeats += 1;
                true
            }
            _ => false,
        };
        if !repeat {
            if let Some((last, _, repeats)) = self.repeated_failure.take().filter(|(_, _, n)| *n > 0) {
                eprintln!("{last} (and {repeats} more like it)");
            }
            self.repeated_failure = Some((text.into(), std::time::Instant::now(), 0));
        }
        // Only a real stop: the worker could not start or restart ("Listen: …"), or its restarts are used up. One
        // chunk's or source's error ("Listen failed: …") is noted, not held (review 23 Oct, finding 11: it read
        // "Transcription stopped. Turn Listening off and on" until the next speech).
        let held = if text.starts_with("Listen: ") || text == super::sources::TRANSCRIPTION_GAVE_UP { Some(("listen", TRANSCRIPTION_STOPPED)) }
            else if text.starts_with("Decisions:") { Some(("decision", DECISIONS_STOPPED)) }
            // The decider owner failed: held (not shown) until the next decision succeeds, while it recovers itself.
            else if text.starts_with("Decision failed") { Some(("decision", "Decision failed")) }
            else if text.starts_with("Speaker activation") { Some(("speakers", "Speakers unavailable")) }
            else if text.starts_with("Screen:") { Some(("screen", SEEING_STOPPED)) }
            else if text.starts_with("Capture:") { Some(("capture", "Capture unavailable")) }
            else if text.starts_with("archive pipeline stopped") { Some(("archive", "Archive paused")) }
            else { None };
        match held {
            Some((component, message)) => self.component_status_reason(component, Some(message.into()), Some(text)),
            None if repeat => {}
            None => crate::diagnostics::record(&self.evidence, "health", "heimdall", serde_json::json!({
                "component": "pipeline", "state": "error", "reason": text,
            })),
        }
        if !repeat {
            self.note_line(text);
        }
    }
    pub fn note_line(&mut self, text: &str) {
        let id = self.next_id;
        self.next_id += 1;
        self.ui(ui::UiUpdate::Transcript(ui::TranscriptLine {
            id,
            label: None,
            segments: vec![ui::Segment::Note(text.into())],
            is_final: true,
        }));
        eprintln!("{text}");
    }
    pub fn settings_result(&mut self, name: &str, error: Option<String>) {
        if self.pending_reload.get(name) != error.as_ref() {
            crate::diagnostics::record(&self.evidence, "health", "heimdall", serde_json::json!({"component":"settings", "setting":name, "state":if error.is_some(){"failed"}else{"recovered"}, "reason":error}));
        }
        if let Some(error) = error {
            self.note_line(&format!("{name}: {error}"));
            self.pending_reload.insert(name.into(), error);
        } else { self.pending_reload.remove(name); }
        self.runtime_settings_ui();
    }
    /// Settings read from settings.json. A stop field that differs from the value the
    /// app last persisted or accepted is fresh: the user's explicit Off/On, ordered at the
    /// latch (closing synchronously) before anything else applies. A pending menu intent
    /// for that reason is newer and wins. The app's own writes are never fresh.
    pub fn apply_settings(&mut self, mut next: Settings) -> Result<()> {
        use agent::delivery::StopReason;
        next.validate()?;
        let file = (next.dry_run, next.paused);
        self.accept_file_stop(file);
        next.dry_run = self.tools.agent_enabled.stopped_for(StopReason::Agent);
        next.paused = self.tools.stopped_for(StopReason::Heimdall);
        self.apply_menu_settings(next)?;
        // A value the latch did not take is rewritten, so relaunch starts from the effective intent.
        // A failed rewrite is retried by `persist_settings`; the settings above are applied either way.
        if (self.settings.dry_run, self.settings.paused) != file { self.save_or_note(); }
        Ok(())
    }
    pub(super) fn accept_file_stop(&mut self, file: (bool, bool)) {
        use agent::delivery::StopReason;
        if file.0 != self.persisted_stop.0 { self.tools.file_intent(StopReason::Agent, !file.0); }
        if file.1 != self.persisted_stop.1 { self.tools.file_intent(StopReason::Heimdall, !file.1); }
        self.persisted_stop = file;
    }
    /// Writes the effective stop intent (the latch), never a lagging in-memory value.
    /// An external stop change the watcher has not read yet is taken first, so this
    /// write cannot erase it. The stop value counts as persisted only once written.
    pub fn save_settings(&mut self) -> Result<()> {
        let path = self.data.join("settings.json");
        if !self.replay && path.exists() {
            if let Ok(file) = Settings::load(&path) { self.accept_file_stop((file.dry_run, file.paused)); }
        }
        let (generation, agent_off, heimdall_off) = self.tools.stop_snapshot();
        self.settings.delivery_generation = generation.agent;
        let mut saved = self.settings.clone();
        saved.delivery_generation = generation.agent;
        saved.dry_run = agent_off;
        saved.paused = heimdall_off;
        if !self.replay {
            if let Err(e) = saved.save(&path).and_then(|()| self.tools.intent_saved_in_settings(generation)) {
                self.save_failed = true;
                return Err(e);
            }
        }
        self.persisted_stop = (saved.dry_run, saved.paused);
        self.save_failed = false;
        self.save_failure_noted = false;
        Ok(())
    }
    /// A live-loop save: the applied settings stay in memory, a failure is noted once
    /// and retried by `persist_settings` until the file matches. Never stops listening.
    pub fn save_or_note(&mut self) {
        if let Err(e) = self.save_settings() { self.note_save_failure(&e); }
    }
    pub fn note_save_failure(&mut self, e: &anyhow::Error) {
        self.component_status("settings-save", Some(format!("Settings save failed; retrying: {e:#}")));
        if !std::mem::replace(&mut self.save_failure_noted, true) { self.note_line(&format!("Settings save failed; retrying: {e:#}")); }
    }
    /// The settings tick: rewrites settings.json after a failed save, or while its stop
    /// value differs from the latch.
    pub fn persist_settings(&mut self) -> Result<()> {
        use agent::delivery::StopReason;
        let tools = &self.tools;
        if !self.save_failed && (tools.stopped_for(StopReason::Agent), tools.stopped_for(StopReason::Heimdall)) == self.persisted_stop { return Ok(()); }
        self.save_settings()
    }
    /// A processed Heimdall menu action (On/Off, setup's Allow All). The callback ordered
    /// the intent; settings follow the latch, which may already hold a newer one.
    pub fn heimdall_intent_processed(&mut self) -> Result<()> {
        use agent::delivery::StopReason;
        self.tools.menu_intent_processed(StopReason::Heimdall);
        self.settings.paused = self.tools.stopped_for(StopReason::Heimdall);
        self.save_settings()
    }
    /// Setup's permission requests finished (`RequestSystemAudio`): setup was requested. The On intent came
    /// earlier, at the click (`SetupAllowed`); settings follow the latch, which may hold a newer Off.
    pub fn setup_requested(&mut self) -> Result<()> {
        use agent::delivery::StopReason;
        self.settings.setup_requested = true;
        self.settings.paused = self.tools.stopped_for(StopReason::Heimdall);
        let saved = self.save_settings();
        // Setup's Allow page stops offering Allow All's actions on Continue.
        self.runtime_settings_ui();
        saved
    }
    /// True when settings.json carries a stop value the app has not accepted yet.
    pub fn stop_settings_fresh(&self, s: &Settings) -> bool {
        (s.dry_run, s.paused) != self.persisted_stop
    }
    /// Capture and screen owners are reconciled by the live loop. Every other setting
    /// shares this path with menu actions and file changes; the archive/session never closes.
    /// Stop fields only follow the latch here; this path never changes it.
    pub fn apply_menu_settings(&mut self, mut next: Settings) -> Result<()> {
        next.validate()?;
        // A removed selection leaves nothing chosen (never another model). The agent's model and the engine are
        // independent (30 Oct): a pick in Agent ▸ Model never runs or stops the engine; the Engine tile and section do.
        next.normalize_agent_model();
        if next.agent_model.is_local() { next.engine_added = true; }
        if next.agent_relay { next.relay_added = true; }
        if next.engine_model != self.settings.engine_model && !next.engine_model.is_empty() {
            crate::inference_engine::Config::effective(&self.data).0.model(&next.engine_model)?;
        }
        // Agent on waits for its pre-flight (pipeline/resilience.rs): until the harness checks out, the agent
        // stays in its dry run whatever the latch, the file or another setting says.
        if self.agent_preflight_pending.is_some() {
            next.dry_run = true;
        }
        next.normalize_archive();
        next.screen.normalize();
        let old = self.settings.clone();
        if old.launch_at_login != next.launch_at_login {
            let result=set_login(next.launch_at_login);
            self.settings_result("Launch at Login",result.err().map(|e|format!("{e:#}")));
        }
        self.settings = next;
        if old.engine_on != self.settings.engine_on || old.engine_model != self.settings.engine_model || old.engine_quant != self.settings.engine_quant {
            self.sync_engine(false);
        }
        // Decider ▸ precision or version (v6/v5): the running decider swaps to it (pipeline/decider.rs).
        let was = decide::precision::Pick::new(old.decider_model, old.decider_precision);
        if was != self.decider_pick() { self.decider_pick_changed(was); }
        self.tools.archive_floor.store(self.settings.free_space_floor_bytes, Ordering::Release);
        self.tools.screen_off.store(self.settings.screen_off, Ordering::Release);
        self.tools.voice_speed.store(self.settings.voice_speed_percent, Ordering::Release);
        if old.speech_voice != self.settings.speech_voice { self.tools.speech_voices.set_choice(self.settings.speech_voice.clone()); } // night 1022 menu
        // Speaking follows its own switch only: with Listening off the agent still talks.
        self.tools.enabled.store(self.settings.speak_replies, Ordering::Release);
        if !self.settings.speak_replies { self.tools.interrupt(); self.tools.clear_speech_failures(); }
        self.tools.speech_policy.set_cue_enabled(self.settings.wake_sound); // speech-policy (night 3)
        self.tools.speech_policy.set_speaking(self.settings.speak_replies);
        if old.islands_hidden != self.settings.islands_hidden { crate::islands::visibility::set(self.settings.islands_hidden) }
        self.tools.speech_policy.set_echo_cancel_off(self.settings.echo_cancel_off); // echo guard (23 Oct)
        crate::islands::streams::set(self.settings.islands_on_streams); // Show Islands on Streams: the floating islands
        if !self.settings.speak_replies && self.settings.drama3_residency == "speaking" && !self.tools.sample_holds_jarvis() {self.tools.drama3.stop();}
        if old.tts_backend != self.settings.tts_backend {
            self.tools.shutdown();
            *self.tools.tts_backend.lock().unwrap() = self.settings.tts_backend.clone();
        }
        if old.parakeet_backend != self.settings.parakeet_backend || old.turn_backend != self.settings.turn_backend {
            *self.listen_settings.lock().unwrap() = self.settings.clone();
            if let Some(worker) = &self.live_worker {
                let result = worker.reload_models();
                if let Err(e) = result { self.settings_result("Transcription Backend", Some(format!("{e:#}"))); }
            }
        }
        if old.speakers_backend != self.settings.speakers_backend {
            for d in self.discord.values() { d.actor.send(crate::speakers_runtime::Work::Backend(self.settings.speakers_backend.clone()))?; }
        }
        if old.agent_model != self.settings.agent_model || old.api_models != self.settings.api_models
            || (old.anthropic_credit_usd, old.anthropic_credit_day) != (self.settings.anthropic_credit_usd, self.settings.anthropic_credit_day) {
            self.tools.api.set_settings(&self.settings);
        }
        // The selected model's own settings (its context slider) are Pi's configuration too; its thinking level is not
        // (the running Pi takes a new level without a restart: `Command::Thinking`).
        let restart_view = |m: Option<&agent::provider::ApiModel>| m.map(|m| agent::provider::ApiModel { thinking: None, ..m.clone() });
        let model_changed = restart_view(old.selected_api_model()) != restart_view(self.settings.selected_api_model())
            // [menu lane] The local model's context budget is Pi's configuration too.
            || (self.settings.agent_model.is_local() && old.engine_context.get(&old.engine_model) != self.settings.engine_context.get(&self.settings.engine_model));
        crate::background_model::publish(&self.settings.background_model, &self.settings.background_off);
        crate::background_model::publish_efforts(&self.settings.background_efforts);
        crate::background_model::publish_tasks(&self.settings.task_models);
        crate::background_model::publish_engine(self.settings.engine_on);
        crate::background_model::publish_fallback_paid(self.settings.background_fallback_paid);
        // A task turned off or given another model leaves the fallback's sign now, not at its next call.
        crate::background_fallback::refresh_routes();
        // Claude Code subscription on or off is a change of agent like a model switch (docs/claude-relay.md).
        if old.dry_run != self.settings.dry_run || old.agent_model != self.settings.agent_model || model_changed || old.agent_relay != self.settings.agent_relay
            || (self.settings.agent_relay && (old.agent_relay_model != self.settings.agent_relay_model || old.agent_relay_account != self.settings.agent_relay_account)) {
            self.tools.interrupt();
            let result = self.restart_agent();
            // Agent back on: a first-run greeting that waited for it goes now (setup_wizard.rs).
            if result.is_ok() && self.settings.first_run_greeting_due {
                if let Some(agent) = self.agent.as_ref() { let _ = agent.barrier(); }
                self.greet_if_due();
            }
            self.settings_result("Agent Session", result.err().map(|e| format!("{e:#}")));
        }
        // Personalise: every reader of the names at once; the agent's prompt after its turn (a Pi reload with the
        // conversation kept, as `reload_me`), the recognizer's name bias with a speech-model reload.
        if old.identity != self.settings.identity {
            crate::identity::publish(&self.settings.identity);
            self.ui(ui::UiUpdate::Identity(self.settings.identity.clone()));
            let change = crate::identity::change(&old.identity, &self.settings.identity);
            if change.names {
                // Agent off: nothing to reload; its next start fills in the names.
                if let Some(agent) = self.agent.as_ref().filter(|a| a.is_running() && !self.settings.dry_run && !self.menu_agent_off()) {
                    if let Err(e) = agent.reload(format!("now called {}, the user {}", self.settings.identity.name, self.settings.identity.user_name)) {
                        self.settings_result("Agent Session", Some(format!("{e:#}")));
                    }
                }
            }
            if change.asr {
                *self.listen_settings.lock().unwrap() = self.settings.clone();
                if let Some(worker) = &self.live_worker {
                    if let Err(e) = worker.reload_models() { self.settings_result("Transcription Backend", Some(format!("{e:#}"))); }
                }
            }
        }
        self.ui(ui::UiUpdate::Settings(self.ui_settings_value()));
        self.runtime_settings_ui();
        // Applied is applied: a journal error (writer down, below the disk floor) must not skip the caller's save.
        if let Err(e) = self.index.event("settings_applied", "heimdall", capture::host_now_ns(), &serde_json::json!({"settings":self.settings,"pending":self.pending_reload.keys().collect::<Vec<_>>()})) {
            eprintln!("settings: applied, journal entry failed: {e:#}");
        }
        Ok(())
    }
    /// The Microphone submenu's rows, kept for the `settings` tool's names.
    pub fn menu_microphones(&mut self, seen: &capture::MicrophoneState) -> ui::Microphones {
        let m = self.settings.microphones_ui(seen);
        self.menu_lists.0 = m.clone();
        m
    }
    /// The App Audio submenu's rows, kept for the `settings` tool's names.
    pub fn menu_apps(&mut self, heard: &[settings::HeardApp]) -> ui::AppAudio {
        let a = self.settings.app_audio_ui(heard);
        self.menu_lists.1 = a.clone();
        a
    }
    /// What the agent's `settings` tool reads (`crate::settings_tool`): the settings and the menu's lists.
    pub fn settings_view(&self) -> crate::settings_tool::View {
        let (engine, _) = crate::inference_engine::Config::effective(&self.data);
        let (relay_models, relay_accounts) = self.tools.relay_catalog.lists();
        crate::settings_tool::View {
            settings: self.settings.clone(),
            agent_off: self.menu_agent_off(),
            listening: !self.tools.stopped_for(agent::delivery::StopReason::Heimdall),
            microphones: self.menu_lists.0.clone(),
            apps: self.menu_lists.1.clone(),
            voices: self.tools.speech_voices.ui_state(),
            presets: self.tools.speech_voices.preset_names(),
            engine_models: engine.models.iter().map(|m| (m.id.clone(), m.label.clone())).collect(),
            relay_models,
            relay_accounts,
            data: self.data.clone(),
            recent_owner: {
                let snapshot = self.tools.send_context.lock().unwrap().snapshot();
                let lines = snapshot.turns.iter().chain(&snapshot.pending).filter(|t| t.speaker.starts_with(decide::names::USER_LABEL));
                let mut lines: Vec<String> = lines.map(|t| t.text.clone()).collect();
                lines.drain(..lines.len().saturating_sub(30));
                lines
            },
        }
    }
    /// The agent turned itself off (or the engine under it) with the user's words: kept in the evidence and the log.
    pub fn record_agent_off(&self, key: &str, words: &str) {
        eprintln!("settings: the agent set {key} off; User asked: {words:?}");
        crate::diagnostics::record(&self.evidence, "settings_tool", "heimdall:agent", serde_json::json!({"event":"agent_off","key":key,"owner_asked":words}));
    }
    pub fn retry_settings(&mut self) -> Result<()> {
        let names: Vec<_> = self.pending_reload.keys().cloned().collect();
        for name in names {
            match name.as_str() {
                "Transcription Backend" => { if let Some(worker) = &self.live_worker { worker.reload_models()?; } },
                "Launch at Login" => { let result=set_login(self.settings.launch_at_login); self.settings_result(&name,result.err().map(|e|format!("{e:#}"))); },
                "Agent Session" => { let result = self.restart_agent(); self.settings_result(&name, result.err().map(|e| format!("{e:#}"))); },
                _ => {},
            }
        }
        Ok(())
    }
    /// The agent's model has a new thinking level: a running agent takes it from its next request.
    pub(super) fn thinking_changed(&mut self) -> Result<()> {
        if let Some(agent) = self.agent.as_ref().filter(|a| a.is_running()) {
            agent.send(Command::Thinking(self.settings.clone()))?;
        }
        Ok(())
    }
    /// The agent restarts whatever Listening says: Listening off never stops it.
    pub(super) fn restart_agent(&mut self) -> Result<()> {
        if self.agent.as_ref().is_some_and(AgentRuntime::is_running) {
            self.agent
                .as_ref()
                .unwrap()
                .send(Command::Restart(self.settings.clone()))?;
        } else {
            self.agent.take();
            self.agent = Some(AgentRuntime::start(
                self.settings.clone(),
                self.data.clone(),
                self.session.clone(),
                self.tools.clone(),
                self.ui,
            )?);
        }
        Ok(())
    }
}
