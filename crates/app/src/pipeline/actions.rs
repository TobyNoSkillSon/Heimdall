//! UI actions from the menu, island and windows (`ui::UiAction`).
use super::*;

impl Pipeline {
    pub fn action(&mut self, a: ui::UiAction) -> Result<()> {
        // Typed "bad suggestion" or "irrelevant" just after a suggestion card: feedback for the suggester, not a message
        // to the agent (suggest.rs).
        if let ui::UiAction::AgentWindow(ui::AgentWindowAction::Send { text, .. }) = &a {
            if self.suggest_feedback(text) {
                return Ok(());
            }
        }
        match a {
            // Setup's own actions (setup_wizard.rs).
            a @ (ui::UiAction::SetupOpened | ui::UiAction::ChooseHome(_) | ui::UiAction::ConfirmHomeMove | ui::UiAction::KeepHome
                | ui::UiAction::SetupDone | ui::UiAction::SetupAt(_) | ui::UiAction::ConnectT3 | ui::UiAction::InstallT3 | ui::UiAction::InstallVella | ui::UiAction::InstallArsenal
                | ui::UiAction::SetHarnessAutonomy(_)) => {
                for update in crate::setup_wizard::action(self, a) { self.ui(update); }
                return Ok(());
            }
            // Drawing mode's Send: attached to the pending utterance, never a wake (drawing.rs).
            ui::UiAction::DrawingSent(shot) => return self.attach_marked(shot),
            // Setup ▸ Arsenal ▸ Use it or Not now: the bridge Pi's extension registers; a running agent restarts to take it.
            ui::UiAction::SetArsenal(on) => {
                let home = heimdall_home::Env::current().user_home;
                let found = crate::setup_wizard::find_arsenal(&t3::install::app_dirs(&home));
                if crate::setup_wizard::set_arsenal(&mut self.settings, on, found.as_ref()) {
                    self.save_or_note();
                    if self.agent.as_ref().is_some_and(|a| a.is_running()) { self.restart_agent()?; }
                }
                self.ui(ui::UiUpdate::ArsenalSetup(crate::setup_wizard::current_arsenal(ui::T3Install::Idle, &self.settings)));
                return Ok(());
            }
            // Setup left the listening page / Getting ready's Retry (model_downloads.rs).
            ui::UiAction::StartModelDownloads => { self.start_model_downloads(); return Ok(()); }
            ui::UiAction::RetryModelDownloads => { self.retry_model_downloads(); return Ok(()); }
            // Getting ready's fixes (setup_ready.rs).
            ui::UiAction::CheckAgent => { self.recheck_agent(); return Ok(()); }
            ui::UiAction::RetryVoice => {
                let route = self.tools.speech_voices.route();
                let key = self.heimdall_voice.key(&self.tools, &route);
                self.heimdall_voice.retry.remove(&key);
                self.phonon_tried = false;
                self.heimdall_voice_tick();
                self.setup_ready_tick();
                return Ok(());
            }
            // A drop onto the notch: held for his words, or alone after a few seconds (drops.rs).
            // Invites on the notch go to the friends bridge. An image's QR code is read on its own thread, so the live loop
            // never waits for it; what is left of the drop comes back as a completion on this thread and takes the same
            // `dropped` path as any other drop.
            ui::UiAction::Dropped(d) => {
                let back = self.internal.0.clone();
                let forward = move |rest: ui::drop::Dropped| {
                    let _ = back.send(super::resilience::Internal::Inference(Box::new(move |p: &mut Pipeline| p.dropped(rest))));
                };
                let Some(d) = crate::friends::intake(d, forward) else { return Ok(()) };
                return self.dropped(d);
            }
            // The open pull-down's composer sent with a drop attached: his typed words and the drop as one message
            // (drops.rs), the same delivery as a notch drop's, so Pi and the relay take it alike.
            ui::UiAction::TypedWithDrop { text, dropped } => return self.typed_with_drop(text, dropped),
            // ---- Night 1022 [notch]: a turn pulled out of the notch becomes floating document islands
            // (`islands::nudge::pull_out_turn`: its reply and its notes, 23 Oct, in the islands folder's docs/, viewer
            // islands from the notch). The islands host's folder (a lab Heimdall's own); no host, nothing is written.
            ui::UiAction::PullOut { text, notes } => {
                let Some(host) = crate::islands::host() else {
                    eprintln!("island: nothing pulled out of the notch: Floating Islands are not running in this Heimdall");
                    return Ok(());
                };
                let notes: Vec<_> = notes.iter().filter_map(|id| {
                    let found = crate::chat_history::note_detail(id);
                    if found.is_none() { eprintln!("island: note {id} is no longer kept; it was not pulled out"); }
                    found
                }).collect();
                let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
                match crate::islands::nudge::pull_out_turn(&host.root, &text, &notes, ms, &ui::load_attachment) {
                    Ok(kept) => for (id, path) in kept { eprintln!("island: pulled out of the notch as {id} ({})", path.display()) },
                    Err(e) => eprintln!("island: could not pull the turn out of the notch: {e:#}"),
                }
                return Ok(());
            }
            // ---- end night 1022 [notch]
            ui::UiAction::Island(r) => {
                crate::diagnostics::record(&self.evidence, "island_report", "heimdall", match r {
                    ui::IslandReport::Shown { id, phase, kind, label, waited_ms } => serde_json::json!({"event":"shown","cue_id":id,"phase":phase,"kind":kind,"label":label,"waited_ms":waited_ms}),
                    ui::IslandReport::Cut { id, phase, kind, label, shown_ms, minimum_ms, why } => serde_json::json!({"event":"cut","cue_id":id,"phase":phase,"kind":kind,"label":label,"shown_ms":shown_ms,"minimum_ms":minimum_ms,"why":why}),
                    ui::IslandReport::Left { id, phase, kind, shown_ms, why } => serde_json::json!({"event":"left","cue_id":id,"phase":phase,"kind":kind,"shown_ms":shown_ms,"why":why}),
                });
                // Visibility telemetry never changes settings or refreshes UI.
                return Ok(());
            }
            // The menu-settings path, like every other tile: persisted, and the engine manager follows the change.
            ui::UiAction::SetEngine(on) => {
                let unchanged=self.settings.engine_on==on;
                // The tile only runs or stops the engine; the agent's model never changes with it (28 Oct 17:26: it
                // switched the agent to the local model and left it waiting while a 27B loaded). Running it lists it.
                let mut next = self.settings.clone(); next.engine_on = on; if on { next.engine_added = true; } self.apply_menu_settings(next)?;
                // A click is explicit intent, including Retry while saved intent stays On.
                // Missing weights never start it (Splash would download them): the tile says so instead.
                if unchanged { self.sync_engine(true); }
            }
            // The user's own words (the island's "Switch to the local model?" Yes, the `settings` tool): the agent on that
            // engine model; a precision not on this Mac downloads first, then is selected (pipeline/engine_models.rs).
            ui::UiAction::SelectEngineModel(id) => self.select_engine_model(id)?,
            // The Engine section's quick-pick: that precision loads; the agent keeps its model.
            ui::UiAction::LoadEngineModel { id, quant } => self.load_engine_model(id, quant)?,
            // Agent ▸ Model ▸ This Mac ▸ Local Splash Engine, confirmed: the agent on whatever the engine has loaded (it
            // waits while nothing is). The engine stays as it is (30 Oct: the agent's picker manages no local model).
            ui::UiAction::SelectLocalAgent => {
                if self.settings.agent_model.is_local() && !self.settings.agent_relay { return Ok(()); }
                let mut next = self.settings.clone();
                next.agent_model = agent::provider::AgentModel::Local;
                next.agent_relay = false;
                next.engine_added = true;
                self.apply_menu_settings(next)?;
            }
            // Decider ▸ a precision confirmed, or Setup's choice: the decider hot-swaps to it (pipeline/decider.rs). A key
            // this build does not offer, or the one already chosen, changes nothing and the menu is told what holds.
            ui::UiAction::SetDeciderPrecision(key) => {
                match decide::precision::Precision::from_key(&key).filter(|p| *p != self.settings.decider_precision) {
                    Some(p) => {
                        let mut next = self.settings.clone(); next.decider_precision = p; self.apply_menu_settings(next)?;
                        // The public edition: a precision not on this Mac downloads (it loads when it lands).
                        self.decider_precision_chosen(p);
                    }
                    None => { self.publish_decider(); return Ok(()); }
                }
            }
            // A section's header folded or opened: the whole folded list, kept for the next launch. Every press gets
            // exactly one echo (the menu holds its own list until each press it sent is answered), an unchanged list too.
            ui::UiAction::SetSectionsCollapsed(sections) => {
                let mut next = self.settings.clone();
                next.menu_collapsed = sections.iter().map(|s| s.key().to_string()).collect();
                if next.menu_collapsed == self.settings.menu_collapsed { self.publish_settings(); return Ok(()); }
                self.apply_menu_settings(next)?;
            }
            // Add Provider ▸ Local Splash Engine: listed under This Mac, nothing else. The engine stays as it was and the
            // agent keeps its model (28 Oct 17:26: this used to be `SetEngine(true)`, which switched the agent).
            ui::UiAction::AddBuiltInEngine => {
                if self.settings.engine_added { return Ok(()); }
                let mut next = self.settings.clone(); next.engine_added = true; self.apply_menu_settings(next)?;
            }
            // Providers ▸ Local Splash Engine ▸ Remove: off This Mac and back in Add Provider, the engine stopped (its
            // weights stay). Refused while the local model is the agent's: the menu is told what is true.
            ui::UiAction::RemoveBuiltInEngine => {
                if self.settings.agent_model.is_local() && !self.settings.agent_relay { self.publish_settings(); return Ok(()); }
                let mut next = self.settings.clone(); next.engine_added = false; next.engine_on = false; self.apply_menu_settings(next)?;
                self.sync_engine(true);
            }
            // The engine's precisions (pipeline/engine_models.rs). The menu sends SetEngineQuant before
            // SelectEngineModel on Confirm, and asks before DeleteEngineModel.
            ui::UiAction::SetEngineQuant { id, quant } => self.set_engine_quant(id, quant)?,
            ui::UiAction::DownloadEngineModel { id, quant, then_select } => self.download_engine_model(id, quant, then_select)?,
            ui::UiAction::CancelEngineDownload { id } => self.cancel_engine_download(&id),
            ui::UiAction::DeleteEngineModel { id, quant } => self.delete_engine_model(id, quant)?,
            // Add Provider's and Setup's "Claude Code subscription": the relay is added as a provider, its row joins Agent ▸
            // Model; the agent keeps its model until the user picks the relay there (30 Oct 01:15).
            // An agent with nothing chosen (Setup's Claude Code tile on a blank start) becomes the relay at once.
            ui::UiAction::ClaudeViaT3 => {
                let adopt = !self.settings.agent_model.is_chosen() && !self.settings.agent_relay;
                if self.settings.relay_added && !adopt { self.publish_settings(); return Ok(()); }
                let mut next = self.settings.clone();
                next.relay_added = true;
                next.agent_relay |= adopt;
                self.apply_menu_settings(next)?;
            }
            // Providers ▸ Claude Code subscription ▸ Remove: back in Add Provider. Refused while it is the agent.
            ui::UiAction::RemoveClaudeViaT3 => {
                if self.settings.agent_relay || !self.settings.relay_added { self.publish_settings(); return Ok(()); }
                let mut next = self.settings.clone();
                next.relay_added = false;
                self.apply_menu_settings(next)?;
            }
            // Agent ▸ Model ▸ Claude Code subscription, confirmed: the relay answers for the agent; `agent_model` stays for
            // when it ends.
            ui::UiAction::SelectAgentRelay => {
                if self.settings.agent_relay { return Ok(()); }
                let mut next = self.settings.clone();
                next.agent_relay = true;
                self.apply_menu_settings(next)?;
            }
            // Agent ▸ Model ▸ Claude Code subscription ▸ a model, confirmed: the relay on that T3 model (a running relay moves to it).
            // A pick, so it is the agent from now on.
            ui::UiAction::SelectRelayModel(model) => {
                if self.settings.agent_relay && self.settings.agent_relay_model.as_deref() == Some(model.as_str()) { return Ok(()); }
                let mut next = self.settings.clone();
                next.agent_relay = true;
                next.agent_relay_model = Some(model);
                self.apply_menu_settings(next)?;
            }
            // Agent ▸ Model ▸ Claude Code subscription ▸ an account, confirmed: the relay on that T3 Claude instance (`None`:
            // Automatic); it keeps its model, and a running relay moves to the account (settings_apply.rs).
            ui::UiAction::SelectRelayAccount(account) => {
                if self.settings.agent_relay && self.settings.agent_relay_account == account { return Ok(()); }
                let mut next = self.settings.clone();
                next.agent_relay = true;
                next.agent_relay_account = account;
                self.apply_menu_settings(next)?;
            }
            // Stay on the replacement after a failure (`emergency_recovery`): account and model in one change, so the relay
            // restarts once and `Emergency::choose` finds what it already runs (no move).
            ui::UiAction::SelectRelayRoute { model, account } => {
                let mut next = self.settings.clone();
                next.agent_relay = true;
                next.agent_relay_account = Some(account);
                if model.is_some() { next.agent_relay_model = model; }
                if next.agent_relay == self.settings.agent_relay && next.agent_relay_account == self.settings.agent_relay_account && next.agent_relay_model == self.settings.agent_relay_model { return Ok(()); }
                self.apply_menu_settings(next)?;
            }
            ui::UiAction::SelectApiModel { provider, id } => {
                // Only an added model that can run is selectable; the menu disables the others and says why.
                // A refused pick sends the settings back, so the menu shows what really runs, not the row clicked.
                let refused = |me: &mut Self| { me.publish_settings(); Ok(()) };
                let Some(model) = self.settings.api_models.iter().find(|m| m.key() == (provider.as_str(), id.as_str())) else { return refused(self) };
                if model.unsupported().is_some() { return refused(self); }
                if let Some(p) = model.provider_info() {
                    if !agent::provider::has_credential(p) { return refused(self); }
                }
                let mut next = self.settings.clone();
                next.agent_model = agent::provider::AgentModel::Api { provider, id };
                next.agent_relay = false;
                self.apply_menu_settings(next)?;
            }
            ui::UiAction::SetApiModelAdded { provider, id, on } => {
                let mut next = self.settings.clone();
                if on {
                    if next.api_models.iter().any(|m| m.key() == (provider.as_str(), id.as_str())) { return Ok(()); }
                    let Some(model) = self.tools.api.catalog_model(&provider, &id) else { return Ok(()) };
                    if model.unsupported().is_some() { return Ok(()); }
                    next.api_models.push(model);
                } else {
                    next.api_models.retain(|m| m.key() != (provider.as_str(), id.as_str()));
                }
                self.apply_menu_settings(next)?;
            }
            // Audit 27 Oct, F2: added rows were frozen as captured; the account's list now reaches them. Metadata only:
            // the running Pi keeps its configuration; the figures are saved and handed to the agent's supervisor (as a
            // thinking change is, `Command::Thinking`), so its next start (a model, slider or thinking change, a reload, a
            // crash restart) uses them, and a catalog load never interrupts a turn.
            ui::UiAction::RefreshApiModels { provider } => {
                if provider != agent::provider::CHATGPT { return Ok(()); }
                let refreshed: Vec<agent::provider::ApiModel> = self.settings.api_models.iter().map(|m| {
                    let fresh = (m.provider == provider).then(|| self.tools.api.catalog_model(&m.provider, &m.id)).flatten();
                    fresh.map_or_else(|| m.clone(), |f| m.refreshed(&f))
                }).collect();
                if refreshed == self.settings.api_models { return Ok(()); }
                self.settings.api_models = refreshed;
                self.tools.api.set_settings(&self.settings);
                self.thinking_changed()?;
                self.save_or_note();
            }
            ui::UiAction::SetApiModelContext { provider, id, tokens } => {
                let mut next = self.settings.clone();
                let Some(model) = next.api_models.iter_mut().find(|m| m.key() == (provider.as_str(), id.as_str())) else { return Ok(()) };
                // Only one of the slider's stops; anything else snaps to the nearest.
                model.context_budget = Some(model.snap(tokens));
                self.apply_menu_settings(next)?;
            }
            ui::UiAction::SetApiModelThinking { provider, id, level } => {
                if !agent::provider::THINKING_ORDER.contains(&level.as_str()) { return Ok(()); }
                let mut next = self.settings.clone();
                let Some(model) = next.api_models.iter_mut().find(|m| m.key() == (provider.as_str(), id.as_str())) else { return Ok(()) };
                if model.thinking.as_deref() == Some(level.as_str()) { return Ok(()); }
                model.thinking = Some(level);
                let live = next.agent_model.api() == Some((provider.as_str(), id.as_str()));
                self.apply_menu_settings(next)?;
                if live { self.thinking_changed()?; }
            }
            // [menu lane] The built-in engine's context slider: one of its stops for the current model.
            ui::UiAction::SetEngineContext(tokens) => {
                // Kept per engine model: with none picked there is nothing to keep it for.
                if self.settings.engine_model.is_empty() { return Ok(()); }
                let mut next = self.settings.clone();
                if next.engine_context.get(&next.engine_model) == Some(&tokens) { return Ok(()); }
                next.engine_context.insert(next.engine_model.clone(), tokens);
                self.apply_menu_settings(next)?;
            }
            // Background Tasks ▸ Model: every background job's model from its next pass.
            ui::UiAction::SetBackgroundModel { provider, id } => {
                let Some(model) = crate::background_model::BackgroundModel::from_ui(&provider, &id) else { return Ok(()) };
                if self.settings.background_model == model { return Ok(()); }
                let mut next = self.settings.clone();
                next.background_model = model;
                self.apply_menu_settings(next)?;
            }
            // Background Tasks ▸ Model ▸ a model's thinking level: that model's calls run at it from the next one (an
            // empty level: each job's own effort again).
            ui::UiAction::SetBackgroundEffort { provider, id, level } => {
                let Some(pick) = crate::background_model::BackgroundEffort::from_ui(&provider, &id, &level) else { return Ok(()) };
                let efforts = crate::background_model::with_effort(&self.settings.background_efforts, &pick);
                if efforts == self.settings.background_efforts { return Ok(()); }
                let mut next = self.settings.clone();
                next.background_efforts = efforts;
                self.apply_menu_settings(next)?;
            }
            // Background Tasks ▸ a task's own Model: that task's calls run on it from its next pass; an empty provider puts
            // the task back on Background Tasks ▸ Model. A model the menu would not offer is refused.
            ui::UiAction::SetTaskModel { task, provider, id } => {
                if !crate::background_model::reads_own_model(task) { return Ok(()); }
                let model = if provider.is_empty() { None } else {
                    let Some(m) = crate::background_model::BackgroundModel::from_ui(&provider, &id) else { return Ok(()) };
                    // Suggestions runs a tool loop: never on T3.
                    if !crate::background_model::task_accepts(task, &m) { return Ok(()); }
                    Some(m)
                };
                let models = crate::background_model::with_task_model(&self.settings.task_models, task, model);
                if models == self.settings.task_models { return Ok(()); }
                let mut next = self.settings.clone();
                next.task_models = models;
                self.apply_menu_settings(next)?;
            }
            // The background fallback's paid step (the Anthropic API key): the user's money, off by default; the `settings`
            // tool sends it.
            ui::UiAction::SetBackgroundFallbackPaid(on) => {
                if self.settings.background_fallback_paid == on { return Ok(()); }
                let mut next = self.settings.clone();
                next.background_fallback_paid = on;
                self.apply_menu_settings(next)?;
            }
            // Background Tasks ▸ a task's toggle: the job skips its passes while off.
            ui::UiAction::SetBackgroundTask { task, on } => {
                let mut next = self.settings.clone();
                next.background_off.retain(|k| k != task.key());
                if !on { next.background_off.push(task.key().into()); }
                if next.background_off == self.settings.background_off { return Ok(()); }
                self.apply_menu_settings(next)?;
            }
            ui::UiAction::SetEngineThinking(level) => {
                if !agent::provider::LOCAL_THINKING.contains(&level.as_str()) || self.settings.engine_model.is_empty() { return Ok(()); }
                let mut next = self.settings.clone();
                if next.engine_thinking.insert(next.engine_model.clone(), level.clone()).as_deref() == Some(level.as_str()) { return Ok(()); }
                let live = next.agent_model.is_local();
                self.apply_menu_settings(next)?;
                if live { self.thinking_changed()?; }
            }
            ui::UiAction::SetProviderCredit { provider, usd } => {
                if provider != agent::provider::ANTHROPIC || !ui_credit_choice(usd) { return Ok(()); }
                let mut next = self.settings.clone();
                next.anthropic_credit_usd = usd;
                self.apply_menu_settings(next)?;
            }
            // A provider connected: its default model joins the list. The agent keeps a model it has: a connection is not
            // a pick (30 Oct 01:2x; until then it became the agent's model). An agent with nothing chosen (the public
            // edition's blank start: Setup's Codex or OpenCode Go tile, a first key) takes it. A catalog not loaded yet
            // leaves the offer waiting; the hub sends it again when the catalog loads (`Hub::offer_default`).
            ui::UiAction::AddProviderDefault { provider } => {
                let Some(p) = agent::provider::provider(&provider) else { return Ok(()) };
                let hub = self.tools.api.clone();
                if !agent::provider::has_credential(p) { hub.default_taken(p.id); return Ok(()); }
                let adopt = !self.settings.agent_model.is_chosen() && !self.settings.agent_relay;
                let Some(model) = connection_default(&self.settings.api_models, p, hub.default_model(p.id).as_deref(), |id| hub.catalog_model(p.id, id), hub.catalog_rows(p.id)) else { return Ok(()) };
                hub.default_taken(p.id);
                let mut next = self.settings.clone();
                if !next.api_models.iter().any(|m| m.key() == model.key()) { next.api_models.push(model.clone()); }
                if adopt { next.agent_model = agent::provider::AgentModel::Api { provider: model.provider.clone(), id: model.id.clone() }; }
                if next.api_models == self.settings.api_models && next.agent_model == self.settings.agent_model { return Ok(()); }
                self.apply_menu_settings(next)?;
            }
            ui::UiAction::StartProviderLogin { provider } => {
                self.tools.api.start_login(provider);
                return Ok(());
            }
            ui::UiAction::CancelProviderLogin { provider } => {
                self.tools.api.cancel_login(&provider);
                return Ok(());
            }
            // ---- speech voice (night 1022 menu) ----
            ui::UiAction::ConnectProviderKey { provider, key } if provider == ui::speech_voice::ELEVENLABS => {
                self.tools.speech_voices.connect_elevenlabs(key);
                return Ok(());
            }
            // Personalise ▸ the name, the user's name, the wake words or the mark: applied live and saved.
            ui::UiAction::Identity(action) => {
                let mut next = self.settings.clone();
                match next.identity.apply(&action) {
                    Ok(true) => { self.apply_menu_settings(next)?; self.save_or_note(); }
                    Ok(false) => {}
                    Err(e) => self.note_line(&format!("Personalise: {e}")),
                }
                return Ok(());
            }
            ui::UiAction::SpeechVoice(action) => {
                let mut next = self.settings.clone();
                if next.speech_voice.apply(&action) { self.apply_menu_settings(next)?; }
                match action {
                    ui::speech_voice::SpeechVoiceAction::DisconnectElevenLabs => {
                        if let Err(e) = self.tools.speech_voices.disconnect_elevenlabs() { self.note_line(&format!("Disconnect failed: {e:#}")); }
                    }
                    ui::speech_voice::SpeechVoiceAction::DisconnectOwnEngine => {
                        if let Err(e) = self.tools.speech_voices.disconnect_own() { self.note_line(&format!("Disconnect failed: {e:#}")); }
                    }
                    ui::speech_voice::SpeechVoiceAction::RefreshOwnEngine => {
                        #[cfg(not(test))]
                        if !self.replay { self.tools.speech_voices.refresh_own(); }
                    }
                    ui::speech_voice::SpeechVoiceAction::RefreshElevenLabs | ui::speech_voice::SpeechVoiceAction::SetApi(true) => {
                        #[cfg(not(test))]
                        if !self.replay { self.tools.speech_voices.refresh_elevenlabs(); }
                    }
                    _ => {}
                }
                // "The sample of the voice should be played" when he clicks a voice.
                // Tests never reach the speaker or load a voice.
                #[cfg(not(test))]
                if matches!(action, ui::speech_voice::SpeechVoiceAction::SetLocal(_) | ui::speech_voice::SpeechVoiceAction::SetPreset(_)
                    | ui::speech_voice::SpeechVoiceAction::SetElevenLabsVoice { .. } | ui::speech_voice::SpeechVoiceAction::SetOwnVoice { .. }
                    | ui::speech_voice::SpeechVoiceAction::SetApi(true)) && !self.replay {
                    self.tools.preview_voice();
                }
                self.tools.speech_voices.publish();
                return Ok(());
            }
            // ---- end speech voice ----
            ui::UiAction::ConnectProviderKey { provider, key } => {
                self.tools.api.connect_key(provider, key);
                return Ok(());
            }
            // [voices] Speaking ▸ Voice ▸ Own Engine: a local OpenAI-compatible speech server.
            ui::UiAction::ConnectProviderEndpoint { provider, url, key } if provider == ui::speech_voice::OWN_VOICE => {
                self.tools.speech_voices.connect_own(url, key);
                return Ok(());
            }
            // [providers] An OpenAI-compatible server or the user's own local engine.
            ui::UiAction::ConnectProviderEndpoint { provider, url, key } => {
                self.tools.api.connect_endpoint(provider, url, key);
                return Ok(());
            }
            ui::UiAction::DisconnectProvider { provider } => {
                if let Err(e) = self.tools.api.disconnect(&provider) { self.note_line(&format!("Disconnect failed: {e:#}")); }
            }
            ui::UiAction::RefreshApiCatalog => {
                // The menu opened: which engine precisions are on this Mac now; old reasons on their rows go.
                self.engine_menu_opened();
                #[cfg(not(test))]
                if !self.replay { self.tools.api.refresh(false); self.tools.emergency.refresh_relay_models(); self.tools.refresh_relay_catalog(); }
                // The menu opened: "Bug Notes (N)" counts the inbox as it is now (the night round may have cleared it).
                if self.bugs.refresh() { self.ui(ui::UiUpdate::BugNotes(self.bugs.listed.clone())); }
                return Ok(());
            }
            ui::UiAction::SetDryRun(_) => {
                // The callback already ordered this intent; settings follow the latch.
                use agent::delivery::StopReason;
                self.tools.menu_intent_processed(StopReason::Agent);
                let off = self.tools.agent_enabled.stopped_for(StopReason::Agent);
                if !off && self.settings.dry_run && !self.replay {
                    // Agent pre-flight: on is accepted only once the harness can start (pipeline/resilience.rs).
                    self.start_preflight();
                } else {
                    if off {
                        self.cancel_preflight();
                        let cue = self.health.clear("agent");
                        self.health_cue(cue);
                        self.ui(ui::UiUpdate::Health(ui::HealthCue::Clear { key: "harness".into() }));
                    }
                    let mut next = self.settings.clone(); next.dry_run = off;
                    self.apply_menu_settings(next)?;
                }
            }
            ui::UiAction::Health { control: ui::HealthControl::ShowArchive, .. } => {
                std::process::Command::new("/usr/bin/open").arg(self.data.join("archive")).spawn()?;
            }
            ui::UiAction::SetArchiveFormat(_) => {
                self.settings.archive_format = settings::ArchiveFormat::NativeLossless;
                self.settings.archive_choice_pending = false;
            }
            ui::UiAction::SetRetentionDays(_) => {
                self.settings.retention_days = 0;
            }
            ui::UiAction::RestartSession => {
                self.restart_agent()?;
            }
            // speech-policy (night 3): Speaking ▸ Wake Sound.
            ui::UiAction::SetWakeSound(b) => {
                let mut next = self.settings.clone(); next.wake_sound = b; self.apply_menu_settings(next)?;
            }
            // The user's live labels for the decider (hotkeys.rs): not settings, so nothing to save.
            ui::UiAction::ForceWake => return self.force_wake(),
            ui::UiAction::ForceSuggest => return self.force_suggest(),
            ui::UiAction::StopSpeaking => return self.stop_press(),
            ui::UiAction::SetIslandsShown(shown) => {
                let mut next = self.settings.clone(); next.islands_hidden = !shown; self.apply_menu_settings(next)?;
            }
            // Show Islands on Streams: the UI has already applied it to its windows; this keeps it.
            ui::UiAction::SetIslandsOnStreams(b) => {
                let mut next = self.settings.clone(); next.islands_on_streams = b; self.apply_menu_settings(next)?;
            }
            ui::UiAction::SetSpeakReplies(b) => {
                let mut next = self.settings.clone(); next.speak_replies = b; self.apply_menu_settings(next)?;
            }
            ui::UiAction::SetVoiceSpeed(percent) => {
                let mut next = self.settings.clone();
                next.voice_speed_percent = percent.clamp(speak::stretch::MIN_PERCENT, speak::stretch::MAX_PERCENT);
                self.apply_menu_settings(next)?;
                // "whenever I change the slider, the sample sound should be … shown like how fast
                // do you actually speak".
                if !self.replay {
                    if let Err(e) = self.tools.play_voice_sample() {
                        crate::diagnostics::record(&self.evidence, "voice_sample", "heimdall:voice", serde_json::json!({"error":format!("{e:#}")}));
                    }
                }
            }
            // The ack's delay, the island's with it (the `settings` tool's `ack_delay`).
            ui::UiAction::SetAckDelay(ms) => {
                let mut next = self.settings.clone(); next.ack_delay_ms = ms.min(crate::settings::MAX_ACK_DELAY_MS); self.apply_menu_settings(next)?;
            }
            // Standard or Fast (the `settings` tool's `agent_speed`); Pi reads it per request.
            ui::UiAction::SetAgentSpeed(speed) => {
                let mut next = self.settings.clone(); next.agent_speed = speed; self.apply_menu_settings(next)?;
            }
            // [chrome] n1022 stage 3: a double-click on a voice's label in the transcript, named.
            ui::UiAction::NameTranscriptSpeaker { line_id, source, speaker, name } => {
                self.name_transcript_speaker(line_id, &source, &speaker, &name)?;
            }
            // The name entry window's typed name (`UiUpdate::VoiceNameEntry`; the island's voice cards now type it inline).
            ui::UiAction::NameVoice { code, name } => {
                self.rename_voice(&code, &name)?;
            }
            ui::UiAction::UnignoreSpeaker { id } => {
                let shared = self.enrollment_lock.clone();
                let _guard = shared.lock().unwrap();
                let path = self.data.join("speakers.json");
                let mut store = self.load_store()?;
                // A forgotten coded voice (by its code) or a voice ignored on the old prompt.
                if store.set_irrelevant(&id, false) || store.unignore(&id) {
                    store.save(&path)?;
                    for d in self.discord.values() {
                        d.actor.send(crate::speakers_runtime::Work::Store(store.clone()))?;
                    }
                }
                drop(_guard);
                self.send_speakers();
            }
            // ---- end Don't ask again
            ui::UiAction::ForgetSpeaker { id } => {
                let shared = self.enrollment_lock.clone();
                let _guard = shared.lock().unwrap();
                let path = self.data.join("speakers.json");
                let mut store = self.load_store()?;
                store.names.remove(&id);
                store.save(&path)?;
                self.rename_recovered(&id,"Unknown")?;
                for d in self.discord.values() {
                    d.actor
                        .send(crate::speakers_runtime::Work::Store(store.clone()))?;
                    d.actor
                        .send(crate::speakers_runtime::Work::Forget(id.clone()))?;
                }
                drop(_guard);
                // The listened guests follow the file (listen_to.rs).
                self.send_speakers();
            }
            ui::UiAction::RenameSpeaker { id, name } => {
                self.rename_voice(&id, &name)?;
            }
            ui::UiAction::ListenToSpeaker { id, on } => {
                // A refused switch (the voice renamed or forgotten meanwhile) goes back to what the file says.
                if let Err(e) = self.set_listening(&id, on, "menu") {
                    self.send_speakers();
                    return Err(e);
                }
            }
            // A partial pick on a suggester card is kept by the island until the card is complete: nothing to do here.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Pick { request_id, .. })
                if request_id.starts_with(crate::suggester::CARD_PREFIX) => {}
            // The suggester's card (suggest.rs): a pick wakes the agent with the handoff; "Not now" only paces.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id, answers, .. })
                if request_id.starts_with(crate::suggester::CARD_PREFIX) =>
            {
                self.suggest_answer(&request_id, &answers)?;
            }
            // A voice card's name field: each keystroke is a pick the island keeps until Return; nothing to do here.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Pick { request_id, .. })
                if request_id.starts_with(crate::voice_namer::CARD_PREFIX) || request_id.starts_with(crate::voice_suggest::CARD_PREFIX) => {}
            // Esc in its name field put the voice card away: the namer's next card may come.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Dismiss { request_id })
                if request_id.starts_with(crate::voice_namer::CARD_PREFIX) || request_id.starts_with(crate::voice_suggest::CARD_PREFIX) =>
            {
                self.voice_card_dismissed(&request_id);
            }
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id, answers, .. })
                if request_id.starts_with(crate::voice_namer::CARD_PREFIX) || request_id.starts_with(crate::voice_suggest::CARD_PREFIX) =>
            {
                // The voice card has one question: its label is the choice, or the name typed in its field.
                let choice = answers.first().and_then(|a| a.labels.first()).cloned().unwrap_or_default();
                let typed = answers.first().and_then(|a| a.other.clone());
                self.voice_card_answer(&request_id, &choice, typed.as_deref())?;
            }
            // The recovery card and the return question (`emergency_recovery`): the controller acts on the answer; a
            // local-model pick is switched to here, by the menu's own path.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id, answers, .. })
                if request_id.starts_with(crate::emergency_recovery::PREFIX) || request_id.starts_with(crate::emergency_recovery::RETURN_PREFIX) =>
            {
                let choice = answers.first().and_then(|a| a.labels.first()).cloned().unwrap_or_default();
                if let Some(model) = self.tools.emergency.answer(&request_id, &choice) {
                    // Pi failed: the hand-back returns to its model. The relay itself was the agent: the idle API model
                    // is not what ran, so nothing offers it later.
                    if self.settings.agent_relay { self.tools.emergency.no_fallback(); }
                    else { self.tools.emergency.local_fallback(self.settings.selected_api_model().cloned()); }
                    self.action(ui::UiAction::SelectEngineModel(model))?;
                }
            }
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Dismiss { request_id })
                if request_id.starts_with(crate::emergency_recovery::PREFIX) || request_id.starts_with(crate::emergency_recovery::RETURN_PREFIX) =>
            {
                self.tools.emergency.answer(&request_id, "");
            }
            // Nothing can cover (`emergency_local`): Yes switches to the local model, No waits for the quota.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id, answers, .. })
                if request_id.starts_with(crate::emergency_local::CARD_PREFIX) =>
            {
                let choice = answers.first().and_then(|a| a.labels.first()).cloned().unwrap_or_default();
                self.emergency_local_answer(&request_id, &choice)?;
            }
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Dismiss { request_id })
                if request_id.starts_with(crate::emergency_local::CARD_PREFIX) =>
            {
                self.emergency_local_answer(&request_id, crate::emergency_local::NO)?;
            }
            // The main model answers again after the local stand-in: Yes picks it, as the user's own pick in the menu would.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Answer { request_id, answers, .. })
                if request_id.starts_with(crate::emergency_local::BACK_PREFIX) =>
            {
                self.ui(ui::UiUpdate::DismissAgentNotice(request_id.clone()));
                let choice = answers.first().and_then(|a| a.labels.first()).cloned().unwrap_or_default();
                if crate::emergency_local::open_back(&request_id) {
                    match crate::emergency_local::take_back(&request_id, &choice) {
                        Some(back) => {
                            eprintln!("emergency: User chose to go back to {}", back.id);
                            return self.action(ui::UiAction::SelectApiModel { provider: back.provider, id: back.id });
                        }
                        None => { eprintln!("emergency: User stays on the local model"); self.tools.emergency.keep_local(); }
                    }
                }
                return Ok(());
            }
            // Closing the card is a No.
            ui::UiAction::AgentNotice(ui::agent_notice::AgentNoticeAction::Dismiss { request_id })
                if request_id.starts_with(crate::emergency_local::BACK_PREFIX) =>
            {
                if crate::emergency_local::open_back(&request_id) {
                    let _ = crate::emergency_local::take_back(&request_id, crate::emergency_local::NO);
                    self.tools.emergency.keep_local();
                }
                return Ok(());
            }
            ui::UiAction::AgentNotice(action) => {
                if let Some(runtime) = &self.agent { runtime.notice_action(action)?; }
            }
            ui::UiAction::AgentWindow(action) => {
                let runtime=self.agent.as_ref().context("Agent is not running")?;
                runtime.window_action(action)?;
            }
            ui::UiAction::ShowArchive => {
                std::process::Command::new("/usr/bin/open")
                    .arg(self.data.join("archive"))
                    .spawn()?;
            }
            ui::UiAction::SetLaunchAtLogin(b) => {
                let mut next = self.settings.clone(); next.launch_at_login = b; self.apply_menu_settings(next)?;
            }
            _ => {}
        }
        self.runtime_settings_ui();
        self.save_or_note();
        Ok(())
    }
}

/// Island cards the app itself shows and answers in `Pipeline::action` (suggestions, relay recovery and return, the
/// local-model cards, voice names). main.rs's `ui_action_callback` sends their answers, picks and dismissals to the
/// live loop; every other card id is an agent question (bug note 2026-11-05T02-53-27: they all went to the agent's
/// question path and failed with ASK_NOT_FOUND). Friends' cards are answered in main.rs itself.
pub(crate) fn app_owns_card(request_id: &str) -> bool {
    [
        crate::suggester::CARD_PREFIX,
        crate::emergency_recovery::PREFIX,
        crate::emergency_recovery::RETURN_PREFIX,
        crate::emergency_local::CARD_PREFIX,
        crate::emergency_local::BACK_PREFIX,
        crate::voice_namer::CARD_PREFIX,
        crate::voice_suggest::CARD_PREFIX,
    ]
    .iter()
    .any(|p| request_id.starts_with(p))
}

impl Pipeline {
    /// The `emergency_local` card's answer. Yes: the main model is remembered for the hand-back
    /// (`Emergency::local_fallback`), then the engine's model is picked as the menu picks it (the engine starts, and
    /// downloads the model first when it is missing). No (or the card dismissed): the quiet waiting line.
    pub(super) fn emergency_local_answer(&mut self, request_id: &str, choice: &str) -> Result<()> {
        self.ui(ui::UiUpdate::DismissAgentNotice(request_id.into()));
        let Some(answer) = crate::emergency_local::take_answer(request_id, choice) else { return Ok(()) };
        match answer {
            crate::emergency_local::Answer::Switch(offer) => {
                eprintln!("emergency: User chose the local model ({}) while no relay covers", offer.model);
                self.tools.emergency.local_fallback(self.settings.selected_api_model().cloned());
                self.action(ui::UiAction::SelectEngineModel(offer.model))
            }
            crate::emergency_local::Answer::Wait(reset) => {
                self.ui(ui::UiUpdate::AgentNotice(crate::emergency_local::waiting(reset)));
                Ok(())
            }
        }
    }
}

/// The model a connection offers (`UiAction::AddProviderDefault`): the provider's default (ChatGPT's probed model, else
/// `connect_default`) as already added, as the catalog describes it, or Anthropic's offline row; when the loaded
/// catalog lacks it, the catalog's first model that can run. `None`: the catalog has not loaded yet.
pub(crate) fn connection_default(
    added: &[agent::provider::ApiModel],
    p: &agent::provider::ProviderInfo,
    default: Option<&str>,
    catalog_model: impl Fn(&str) -> Option<agent::provider::ApiModel>,
    catalog: Option<Vec<agent::provider::ApiModel>>,
) -> Option<agent::provider::ApiModel> {
    if let Some(id) = default {
        if let Some(m) = added.iter().find(|m| m.key() == (p.id, id)) { return Some(m.clone()); }
        if let Some(m) = catalog_model(id).or_else(|| agent::provider::ApiModel::anthropic(id).filter(|_| p.id == agent::provider::ANTHROPIC)) { return Some(m); }
    }
    catalog?.into_iter().find(|m| m.unsupported().is_none())
}
