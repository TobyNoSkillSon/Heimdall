//! Inference Engine ▸ API Providers on the app side: each provider's key state, the "+" picker's catalog and the
//! rows the menu shows. Network work runs on short-lived threads with explicit deadlines; every change republishes
//! one `UiUpdate::ApiProviders`. Keys pass through here in memory only (`agent::provider` keeps them).
use agent::provider::{self, AgentModel, ApiModel, KeyCheck, ProviderInfo, Usage};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A failed catalog load is retried on the next menu open; a good one is refreshed after this long.
const CATALOG_FRESH: Duration = Duration::from_secs(6 * 3600);
/// models.dev's whole-catalog file (≈6 MB) is reused from disk for a day.
const METADATA_FRESH: Duration = Duration::from_secs(24 * 3600);

#[derive(Default)]
struct State {
    selection: AgentModel,
    models: Vec<ApiModel>,
    auth: BTreeMap<&'static str, ui::ProviderAuth>,
    catalog: BTreeMap<&'static str, (Instant, Result<Vec<ApiModel>, String>)>,
    fetching: BTreeSet<&'static str>,
    /// The pending browser sign-in per provider; setting it stops the polling.
    logins: BTreeMap<&'static str, Arc<std::sync::atomic::AtomicBool>>,
    /// The last failed sign-in per provider, for the menu (cleared by the next attempt or a connection).
    login_errors: BTreeMap<&'static str, String>,
    /// [providers] The pending ChatGPT sign-in's consent page (the panel's Open Browser has it too).
    login_urls: BTreeMap<&'static str, String>,
    /// [providers] The model a new connection selects, when its sign-in found it (ChatGPT's probed model).
    defaults: BTreeMap<&'static str, String>,
    /// Anthropic's monthly credit and renewal day (settings), and the spend counted against it.
    credit: (u32, u8),
    ledger: Option<Ledger>,
}

/// Test hooks: a local fake instead of OpenCode and models.dev.
#[derive(Clone, Default)]
pub struct Urls {
    pub models: Option<String>,
    pub metadata: Option<String>,
    pub console: Option<String>,
    /// [providers] A fake OpenAI: its sign-in (`/api/accounts/…`) and API (`/v1/…`) on one origin, and the loopback port
    /// (0: any free one).
    pub chatgpt: Option<(String, u16)>,
}

pub struct Hub {
    state: Mutex<State>,
    ui: Mutex<Option<ui::Ui>>,
    /// Where Pi's runtime (and its pinned catalog) is installed, and the metadata cache.
    data: PathBuf,
    urls: Urls,
    /// Scenario tests only (`fake_chatgpt`): a fake OpenAI for a hub the app built, in place of `urls.chatgpt`.
    chatgpt_fake: Mutex<Option<(String, u16)>>,
    /// Told when a provider's key changes, so a running agent on that provider restarts with it.
    key_changed: Mutex<Option<Box<dyn Fn(&'static str) + Send>>>,
    /// The app's action queue: a first connection asks it to select the provider's default model.
    actions: Mutex<Option<crossbeam_channel::Sender<ui::UiAction>>>,
    pi_catalogs: std::sync::OnceLock<BTreeMap<&'static str, Option<serde_json::Value>>>,
    /// The ChatGPT token keeper is running (`keep_chatgpt_fresh`).
    keeper: std::sync::atomic::AtomicBool,
}

impl Hub {
    pub fn new(ui: Option<ui::Ui>, data: PathBuf, urls: Urls) -> Arc<Self> {
        let hub = Arc::new(Self { state: Mutex::default(), ui: Mutex::new(ui), data, urls, chatgpt_fake: Mutex::new(None), key_changed: Mutex::new(None), actions: Mutex::new(None), pi_catalogs: Default::default(), keeper: Default::default() });
        // [background] The background jobs read the model's entry and the ChatGPT token here (`background_model`); the
        // app makes one hub. Tests' hubs stay out of it.
        if !cfg!(test) {
            crate::background_model::set_hub(&hub);
            // Own Engine was retired into the OpenAI-compatible custom provider: its saved URL and key carry over once.
            match provider::adopt_retired_endpoint() {
                Ok(true) => eprintln!("Providers: Own Engine's URL and key now belong to the OpenAI-compatible custom provider"),
                Ok(false) => {}
                Err(e) => eprintln!("Providers: Own Engine's URL and key not carried over: {e:#}"),
            }
        }
        hub
    }

    pub fn set_ui(&self, ui: Option<ui::Ui>) {
        *self.ui.lock().unwrap() = ui;
    }

    pub fn on_key_changed(&self, f: impl Fn(&'static str) + Send + 'static) {
        *self.key_changed.lock().unwrap() = Some(Box::new(f));
    }

    pub fn set_actions(&self, tx: crossbeam_channel::Sender<ui::UiAction>) {
        *self.actions.lock().unwrap() = Some(tx);
    }

    /// The selection, the added models or the credit changed (or the app started).
    pub fn set_settings(&self, s: &crate::settings::Settings) {
        {
            let mut st = self.state.lock().unwrap();
            st.selection = s.agent_model.clone();
            st.models = s.api_models.clone();
            st.credit = (s.anthropic_credit_usd, s.anthropic_credit_day);
        }
        self.publish();
    }

    /// One provider response's token counts (a reply or a compaction summary): its price joins the day's spend for
    /// prepaid providers. Prices come from the model's metadata; a model without a known price adds nothing.
    pub fn record_usage(&self, model: &ApiModel, usage: &Usage, at_ms: u64) {
        let Some(p) = model.provider_info().filter(|p| p.hour_cache) else { return };
        let Some(cost) = model.request_cost(usage, p.hour_cache).filter(|c| *c > 0.0) else { return };
        let path = self.data.join(SPEND_FILE);
        {
            let mut st = self.state.lock().unwrap();
            let ledger = st.ledger.get_or_insert_with(|| Ledger::load(&path));
            ledger.add(p.id, local_day(at_ms), cost);
            if let Err(e) = ledger.save(&path) { eprintln!("API spend not saved: {e:#}"); }
        }
        self.publish();
    }

    /// One background-job run (`background_call`): its dollars join the day's spend for prepaid providers, like the
    /// agent's, and one line per run (numbers only, no request content) goes to `BACKGROUND_RUNS`.
    pub fn record_background(&self, provider_id: &str, run: &crate::background_call::Run) {
        let runs = self.data.join(BACKGROUND_RUNS);
        let line = format!("{}\n", run.line());
        let written = runs.parent().map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(&runs))
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        if let Err(e) = written { eprintln!("Background run not logged: {e}"); }
        let Some(p) = provider::provider(provider_id).filter(|p| p.hour_cache) else { return };
        if run.usd <= 0.0 { return }
        let path = self.data.join(SPEND_FILE);
        {
            let mut st = self.state.lock().unwrap();
            let ledger = st.ledger.get_or_insert_with(|| Ledger::load(&path));
            ledger.add(p.id, local_day(run.at_ms), run.usd);
            if let Err(e) = ledger.save(&path) { eprintln!("API spend not saved: {e:#}"); }
        }
        self.publish();
    }

    /// The catalog's entry for a listed model (with the metadata captured for Pi), if loaded.
    pub fn catalog_model(&self, provider_id: &str, id: &str) -> Option<ApiModel> {
        let st = self.state.lock().unwrap();
        st.catalog.get(provider_id)?.1.as_ref().ok()?.iter().find(|m| m.id == id).cloned()
    }

    /// Tests: a loaded catalog for `provider_id`, as `load_catalog` keeps one.
    #[cfg(test)]
    pub fn set_catalog(&self, provider_id: &'static str, rows: Vec<ApiModel>) {
        self.state.lock().unwrap().catalog.insert(provider_id, (Instant::now(), Ok(rows)));
    }

    pub fn auth(&self, provider_id: &str) -> Option<ui::ProviderAuth> {
        self.state.lock().unwrap().auth.get(provider_id).cloned()
    }

    fn set_auth(&self, p: &'static ProviderInfo, auth: ui::ProviderAuth) {
        let mut st = self.state.lock().unwrap();
        let mut changed = st.auth.insert(p.id, auth.clone()) != Some(auth.clone());
        if auth == ui::ProviderAuth::Connected { changed |= st.login_errors.remove(p.id).is_some(); }
        drop(st);
        if changed { self.publish(); }
    }

    /// The agent met a provider problem (or recovered from one) while running.
    pub fn runtime_problem(&self, provider_id: &str, problem: Option<String>) {
        let Some(p) = provider::provider(provider_id) else { return };
        let current = self.auth(p.id);
        match problem {
            Some(problem) => self.set_auth(p, ui::ProviderAuth::Problem(problem)),
            // Only a problem clears; Missing/Checking are not the agent's to change.
            None if matches!(current, Some(ui::ProviderAuth::Problem(_))) => self.set_auth(p, ui::ProviderAuth::Connected),
            None => {}
        }
    }

    /// Startup and menu opens: check each provider's key and load its catalog when missing, failed or stale.
    pub fn refresh(self: &Arc<Self>, check_keys: bool) {
        if check_keys && !cfg!(test) { self.keep_chatgpt_fresh(); }
        for p in provider::PROVIDERS {
            if check_keys {
                let hub = self.clone();
                spawn(move || hub.check_saved_key(p));
            }
            let due = {
                let mut st = self.state.lock().unwrap();
                let due = !st.fetching.contains(p.id)
                    && st.catalog.get(p.id).is_none_or(|(at, r)| r.is_err() || at.elapsed() > CATALOG_FRESH);
                if due { st.fetching.insert(p.id); }
                due
            };
            if due {
                let hub = self.clone();
                spawn(move || hub.load_catalog(p));
            }
        }
        self.publish();
    }

    /// Check every saved key or sign-in again (the agent saw a credential failure).
    pub fn recheck(self: &Arc<Self>) {
        for p in provider::PROVIDERS {
            let hub = self.clone();
            spawn(move || hub.check_saved_key(p));
        }
    }

    fn check_saved_key(&self, p: &'static ProviderInfo) {
        let Ok(credential) = provider::credential(p).ok().filter(|_| provider::has_credential(p)).ok_or(()) else { return self.set_auth(p, ui::ProviderAuth::Missing) };
        if self.auth(p.id).is_none() { self.set_auth(p, ui::ProviderAuth::Checking); }
        let chatgpt = (p.id == provider::CHATGPT).then(|| self.chatgpt_origin().map(|(b, _)| format!("{b}/v1/models"))).flatten();
        let auth = match block_on(check_credential(p, &credential, &self.data, chatgpt.as_deref().or(self.urls.models.as_deref()))) {
            Ok(_) => ui::ProviderAuth::Connected,
            Err(problem) => ui::ProviderAuth::Problem(problem),
        };
        self.set_auth(p, auth);
    }

    fn load_catalog(&self, p: &'static ProviderInfo) {
        // OpenCode's list is public and a pasted key is sent along (a Console token is for inference only);
        // Anthropic's needs the key.
        let key = provider::api_key(p).ok().or_else(|| (p.id == provider::CHATGPT).then(|| self.chatgpt_token()).flatten());
        let workspace = key.as_ref().and_then(|_| provider::workspace_id(p));
        let pi = agent::runtime::Runtime::installed().ok().and_then(|r| provider::pi_catalog(&r.root, p));
        // [providers] A provider on the user's own URL lists `<base>/models` once connected.
        let base = provider::base_url(p);
        let custom = base.as_ref().map(|b| provider::models_url_at(p, b));
        let result = block_on(async {
            if p.custom_url && custom.is_none() { anyhow::bail!("{}", provider::KeyMissing(p.name)); }
            let chatgpt_models = (p.id == provider::CHATGPT).then(|| self.chatgpt_origin().map(|(b, _)| format!("{b}/v1/models"))).flatten();
            let models = custom.as_deref().or(chatgpt_models.as_deref()).or(self.urls.models.as_deref());
            let listed = match provider::live_models(p, key.as_deref(), workspace.as_deref(), models).await {
                // A key stored without the workspace it needs (or whose workspace changed): find it, keep it, list again.
                Err(e) if p.workspaces.is_some() && e.to_string().contains("workspace") && key.is_some() => {
                    let key = key.as_deref().unwrap_or_default();
                    match provider::check_key(p, key, workspace.as_deref(), models).await {
                        KeyCheck::Valid { workspace: Some(id) } => {
                            if let Err(e) = provider::save_workspace(p, Some(&id)) { eprintln!("{} workspace not saved: {e:#}", p.name); }
                            provider::live_models(p, Some(key), Some(&id), models).await?
                        }
                        _ => return Err(e),
                    }
                }
                other => other?,
            };
            let metadata = if provider::needs_metadata(&listed, pi.as_ref()) && p.metadata_url.is_some() { self.metadata(p).await } else { None };
            Ok::<_, anyhow::Error>(provider::merge_catalog_at(p, &listed, pi.as_ref(), metadata.as_ref(), base.as_deref()))
        })
        .map_err(|e| e.to_string());
        let stale = {
            let mut st = self.state.lock().unwrap();
            st.fetching.remove(p.id);
            let stale = refreshes(&st.models, p.id, result.as_deref().unwrap_or_default());
            st.catalog.insert(p.id, (Instant::now(), result));
            stale
        };
        // Added rows the catalog now describes better (audit 27 Oct, F2): the app updates its settings.
        if stale {
            if let Some(tx) = self.actions.lock().unwrap().as_ref() { let _ = tx.send(ui::UiAction::RefreshApiModels { provider: p.id.into() }); }
        }
        self.publish();
    }

    /// models.dev's entry for `p`, from a day-old cache or the network; `None` when neither is available.
    async fn metadata(&self, p: &'static ProviderInfo) -> Option<serde_json::Value> {
        let cache = self.data.join(format!("agent/models-dev-{}.json", p.id));
        let fresh = std::fs::metadata(&cache).and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|e| e < METADATA_FRESH));
        if fresh {
            if let Some(v) = std::fs::read(&cache).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                return Some(v);
            }
        }
        let v = provider::fetch_metadata(p, self.urls.metadata.as_deref()).await.ok().filter(|v| v.is_object())?;
        if let Some(parent) = cache.parent() {
            if crate::settings::private_dir(parent).is_ok() {
                let _ = agent::runtime::atomic_write(&cache, &serde_json::to_vec(&v).unwrap_or_default());
            }
        }
        Some(v)
    }

    /// Connect: the browser sign-in. The panel gets the code, the browser opens the approval page (UI), and the
    /// approved session is kept; Cancel stops the polling. OpenCode's device login, with its public CLI client id.
    pub fn start_login(self: &Arc<Self>, provider_id: String) {
        let Some(p) = provider::provider(&provider_id).filter(|p| p.sign_in) else { return };
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let mut st = self.state.lock().unwrap();
            st.login_errors.remove(p.id);
            if let Some(old) = st.logins.insert(p.id, cancel.clone()) { old.store(true, std::sync::atomic::Ordering::Release); }
        }
        self.publish();
        let hub = self.clone();
        if p.id == provider::CHATGPT { return spawn(move || hub.chatgpt_login(p, cancel)); }
        spawn(move || {
            let server = hub.urls.console.clone().unwrap_or_else(|| agent::console::SERVER.into());
            let ui = *hub.ui.lock().unwrap();
            let cancelled = || cancel.load(std::sync::atomic::Ordering::Acquire);
            let result = block_on(async {
                let code = agent::console::start(&server).await?;
                if cancelled() { anyhow::bail!("Sign-in cancelled"); }
                if let Some(ui) = ui { ui.send(ui::UiUpdate::ProviderLogin { provider: p.id.into(), code: code.user_code.clone(), url: code.url.clone() }); }
                let token = agent::console::wait_for_approval(&server, &code, cancelled).await?;
                agent::console::finish(&server, p.id, token).await
            });
            // Cancel is honoured up to the moment the approved session would be kept, never after.
            if cancelled() { return; }
            let result = match result {
                Ok(session) => provider::save_console_session(p, &session).map(|_| ()),
                Err(e) => Err(e),
            };
            {
                let mut st = hub.state.lock().unwrap();
                st.logins.remove(p.id);
                if let Err(e) = &result { st.login_errors.insert(p.id, format!("Sign-in failed: {e:#}")); }
            }
            let result = result.map_err(|e| format!("{e:#}"));
            hub.publish();
            if result.is_ok() {
                hub.set_auth(p, ui::ProviderAuth::Connected);
                hub.key_changed(p.id);
                hub.state.lock().unwrap().catalog.remove(p.id);
                hub.refresh(false);
            }
            if let Some(ui) = ui { ui.send(ui::UiUpdate::ProviderConnected { provider: p.id.into(), result }); }
        });
    }

    /// [providers] Sign in with ChatGPT: the browser opens OpenAI's consent page (the panel has no code to show), the
    /// loopback callback brings the grant back, and the session is kept (under the refresh lock, so a refresh in flight
    /// never saves over it). Every sign-in then runs one tiny test request on the smallest model the account lists
    /// (GPT-6 Luna): a completed one leaves ChatGPT connected, a plan that cannot share its usage is named, a temporary
    /// failure leaves it connected and unverified. Signing in never switches the agent ("I don't actually
    /// want to switch. I just want to see if it works.").
    /// Each step and failure is one `heimdall.log` line, never a code or token.
    fn chatgpt_login(self: &Arc<Self>, p: &'static ProviderInfo, cancel: Arc<std::sync::atomic::AtomicBool>) {
        let log = |text: &str| crate::diagnostics::line("providers", &format!("ChatGPT sign-in: {text}"));
        let ui = *self.ui.lock().unwrap();
        let (auth_base, api_base, port) = self.chatgpt_urls();
        let agent_dir = self.data.join("agent");
        let cancelled = || cancel.load(std::sync::atomic::Ordering::Acquire);
        let saved_client = agent::chatgpt::saved_client(&agent_dir).or_else(|| provider::console_session(p).and_then(|s| s.client_id));
        let mine = Mutex::new(None::<String>);
        let result = block_on(async {
            let host = agent::chatgpt::host_id(&agent_dir)?;
            let login = agent::chatgpt::begin(&auth_base, &host, saved_client.as_deref(), port).await?;
            log(&format!("started on port {port}, {}; the browser opens the consent page", if saved_client.is_some() { "reusing Heimdall's app entry" } else { "registering Heimdall" }));
            self.state.lock().unwrap().login_urls.insert(p.id, login.url.clone());
            *mine.lock().unwrap() = Some(login.url.clone());
            if let Some(ui) = ui { ui.send(ui::UiUpdate::ProviderLogin { provider: p.id.into(), code: String::new(), url: login.url.clone() }); }
            let agent::chatgpt::SignIn { session, plan } = agent::chatgpt::finish(login, cancelled, agent::chatgpt::LOGIN_TIMEOUT).await?;
            let listed = agent::chatgpt::models(&api_base, &session.access).await.unwrap_or_else(|e| { log(&format!("model list failed: {e:#}")); vec![] });
            let model = agent::chatgpt::test_model(&listed);
            log(&format!("{} models listed ({}); one test request on {model}", listed.len(),
                listed.iter().filter_map(|m| m["id"].as_str()).take(8).collect::<Vec<_>>().join(", ")));
            let probed = agent::chatgpt::probe(&api_base, &session.access, &model).await;
            Ok::<_, anyhow::Error>((session, plan, model, probed))
        });
        {
            // This attempt's page only: a newer sign-in may have started since.
            let mut st = self.state.lock().unwrap();
            if st.login_urls.get(p.id) == mine.lock().unwrap().as_ref() { st.login_urls.remove(p.id); }
        }
        if cancelled() { log("cancelled; nothing kept"); return; }
        if result.as_ref().err().is_some_and(|e| e.downcast_ref::<agent::chatgpt::ClientGone>().is_some()) {
            let _ = agent::chatgpt::save_client(&agent_dir, None);
        }
        let result = result.and_then(|(session, plan, model, probed)| {
            agent::console::with_lock(p, &agent_dir, || provider::save_console_session(p, &session))?;
            if let Err(e) = agent::chatgpt::save_client(&agent_dir, session.client_id.as_deref()) { log(&format!("client id not kept: {e:#}")); }
            Ok((plan, model, probed))
        });
        // The test request's verdict: (outcome, problem to name).
        let (outcome, problem) = match result {
            Ok((_, model, Ok(()))) => { log(&format!("signed in and kept; the test request on {model} completed; the agent's model is unchanged")); (Ok(()), None) }
            Ok((plan, model, Err(e))) if is_plan_refusal(&e) => {
                let why = if e.to_string().contains("can't share its usage") { agent::chatgpt::not_eligible(plan.as_deref()) } else { e.to_string() };
                log(&format!("signed in and kept; the test request on {model} was refused: {why}"));
                (Ok(()), Some(why))
            }
            Ok((_, model, Err(e))) => { log(&format!("signed in and kept; the test request on {model} failed, not verified yet: {e:#}")); (Ok(()), None) }
            Err(e) => { log(&format!("failed: {e:#}")); (Err(format!("{e:#}")), None) }
        };
        {
            let mut st = self.state.lock().unwrap();
            st.logins.remove(p.id);
            if let Err(e) = &outcome { st.login_errors.insert(p.id, format!("Sign-in failed: {e}")); }
        }
        self.publish();
        if outcome.is_ok() {
            self.set_auth(p, problem.map_or(ui::ProviderAuth::Connected, ui::ProviderAuth::Problem));
            self.key_changed(p.id);
            self.state.lock().unwrap().catalog.remove(p.id);
            self.refresh(false);
        }
        if let Some(ui) = ui { ui.send(ui::UiUpdate::ProviderConnected { provider: p.id.into(), result: outcome }); }
    }

    /// The model a new connection selects: the one its first request ran on (ChatGPT), else the provider's default.
    pub fn default_model(&self, provider_id: &str) -> Option<String> {
        self.state.lock().unwrap().defaults.get(provider_id).cloned().or_else(|| provider::provider(provider_id)?.connect_default.map(str::to_owned))
    }

    /// [providers] Keeps a ChatGPT sign-in's token fresh with the app's 20-second budget (10 minutes ahead), so Pi's
    /// per-request command, with its 5 seconds, almost never has to rotate it. Ends with the hub.
    pub fn keep_chatgpt_fresh(self: &Arc<Self>) {
        if self.keeper.swap(true, std::sync::atomic::Ordering::AcqRel) { return; }
        let hub = Arc::downgrade(self);
        spawn(move || loop {
            let Some(h) = hub.upgrade() else { return };
            if let Some(p) = provider::provider(provider::CHATGPT).filter(|p| provider::console_session(p).is_some()) {
                if let Err(e) = block_on(agent::console::access_token(p, &h.data.join("agent"), agent::console::Budget::APP)) {
                    if agent::console::is_signed_out(&e) { h.set_auth(p, ui::ProviderAuth::Problem(e.to_string())); }
                }
            }
            drop(h);
            std::thread::sleep(Duration::from_secs(4 * 60));
        });
    }

    /// A fake OpenAI's origin and loopback port (tests), else None: OpenAI itself.
    fn chatgpt_origin(&self) -> Option<(String, u16)> {
        self.chatgpt_fake.lock().unwrap().clone().or_else(|| self.urls.chatgpt.clone())
    }

    /// Scenario tests: the app's own hub signs in to ChatGPT against a fake OpenAI at `base`.
    #[cfg(test)]
    pub(crate) fn fake_chatgpt(&self, base: String, port: u16) {
        *self.chatgpt_fake.lock().unwrap() = Some((base, port));
    }

    /// Scenario tests: the consent page a sign-in in progress opened, as the browser got it.
    #[cfg(test)]
    pub(crate) fn pending_login_url(&self, provider_id: &str) -> Option<String> {
        self.state.lock().unwrap().login_urls.get(provider_id).cloned()
    }

    fn chatgpt_urls(&self) -> (String, String, u16) {
        match self.chatgpt_origin() {
            Some((base, port)) => (base.clone(), format!("{base}/v1"), port),
            None => (agent::chatgpt::AUTH_BASE.into(), agent::chatgpt::API_BASE.into(), agent::chatgpt::PORT),
        }
    }

    /// [background] The background model's entry (format, base URL, limits): an added model, else the loaded catalog's.
    pub fn api_model(&self, provider_id: &str, id: &str) -> Option<ApiModel> {
        let added = self.state.lock().unwrap().models.iter().find(|m| m.provider == provider_id && m.id == id).cloned();
        added.or_else(|| self.catalog_model(provider_id, id))
    }

    /// [background] The token keeper's ChatGPT access token for the background jobs' requests (`background_model`).
    pub fn chatgpt_access_token(&self) -> Option<String> { self.chatgpt_token() }

    /// The signed-in ChatGPT access token, refreshed when near expiry (for the model list).
    fn chatgpt_token(&self) -> Option<String> {
        let p = provider::provider(provider::CHATGPT)?;
        provider::console_session(p)?;
        block_on(agent::console::access_token(p, &self.data.join("agent"), agent::console::Budget::APP)).ok().map(|s| s.access)
    }

    pub fn cancel_login(&self, provider_id: &str) {
        if let Some(flag) = self.state.lock().unwrap().logins.remove(provider_id) {
            flag.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// The key panel's Connect (the fallback): verify against the provider, keep it, tell the panel and the agent.
    pub fn connect_key(self: &Arc<Self>, provider_id: String, key: String) {
        let hub = self.clone();
        spawn(move || {
            let result = hub.connect_key_now(&provider_id, &key);
            if let Some(ui) = *hub.ui.lock().unwrap() { ui.send(ui::UiUpdate::ProviderConnected { provider: provider_id, result }); }
        });
    }

    fn connect_key_now(self: &Arc<Self>, provider_id: &str, key: &str) -> Result<(), String> {
        let p = provider::provider(provider_id).ok_or("Provider not supported")?;
        let key = provider::normalize_key(key).map_err(|e| e.to_string())?;
        // A key that is not scoped to a workspace comes back with the workspace its requests will name.
        let workspace = match block_on(provider::check_key(p, &key, None, self.urls.models.as_deref())) {
            KeyCheck::Valid { workspace } => workspace,
            KeyCheck::Rejected => return Err(provider::rejected_message(p)),
            KeyCheck::Refused(why) => return Err(why),
            KeyCheck::Unreachable => return Err(format!("{} · try again", provider::unreachable_message(p))),
        };
        let first = !provider::has_credential(p);
        provider::save_key(p, &key, workspace.as_deref()).map_err(|e| e.to_string())?;
        self.set_auth(p, ui::ProviderAuth::Connected);
        self.key_changed(p.id);
        // A new connection adds its default model to the list; the agent keeps its model until the user picks another
        // (30 Oct 01:2x: until then a connection made it the agent's).
        if first && p.connect_default.is_some() {
            if let Some(tx) = self.actions.lock().unwrap().as_ref() { let _ = tx.send(ui::UiAction::AddProviderDefault { provider: p.id.into() }); }
        }
        // The list may differ for a subscriber; reload it with the key.
        self.state.lock().unwrap().catalog.remove(p.id);
        self.refresh(false);
        Ok(())
    }

    /// [providers] Connect a custom provider (OpenAI- or Anthropic-compatible): its `/models` must answer with
    /// the key (a keyless engine is asked with `NO_KEY`), then the URL and key are kept and the list reloads.
    pub fn connect_endpoint(self: &Arc<Self>, provider_id: String, url: String, key: String) {
        let hub = self.clone();
        spawn(move || {
            let result = hub.connect_endpoint_now(&provider_id, &url, &key);
            if let Some(ui) = *hub.ui.lock().unwrap() { ui.send(ui::UiUpdate::ProviderConnected { provider: provider_id, result }); }
        });
    }

    fn connect_endpoint_now(self: &Arc<Self>, provider_id: &str, url: &str, key: &str) -> Result<(), String> {
        let p = provider::provider(provider_id).filter(|p| p.custom_url).ok_or("Provider not supported")?;
        let base = provider::normalize_base_url_for(p, url).map_err(|e| e.to_string())?;
        let key = match key.trim() {
            "" if !p.key_required => provider::NO_KEY.to_owned(),
            k => provider::normalize_key(k).map_err(|e| e.to_string())?,
        };
        match block_on(provider::check_key(p, &key, None, Some(&provider::models_url_at(p, &base)))) {
            KeyCheck::Valid { .. } => {}
            KeyCheck::Rejected => return Err(provider::rejected_message(p)),
            KeyCheck::Refused(why) => return Err(why),
            KeyCheck::Unreachable => return Err(format!("{base} unreachable · check the URL")),
        }
        provider::save_endpoint(p, &base, Some(&key)).map_err(|e| e.to_string())?;
        self.set_auth(p, ui::ProviderAuth::Connected);
        self.key_changed(p.id);
        self.state.lock().unwrap().catalog.remove(p.id);
        self.refresh(false);
        Ok(())
    }

    pub fn disconnect(&self, provider_id: &str) -> anyhow::Result<()> {
        let p = provider::provider(provider_id).ok_or_else(|| anyhow::anyhow!("Provider not supported"))?;
        self.cancel_login(p.id);
        // Under the refresh lock: a refresh in flight must not bring the sign-in back.
        agent::console::with_lock(p, &self.data.join("agent"), || provider::forget_key(p))?;
        self.set_auth(p, ui::ProviderAuth::Missing);
        self.key_changed(p.id);
        Ok(())
    }

    fn key_changed(&self, id: &'static str) {
        if let Some(f) = self.key_changed.lock().unwrap().as_ref() { f(id); }
    }

    pub fn snapshot(&self) -> ui::ApiProviders {
        // Pi's pinned catalogs (they decide the thinking levels of models outside the Claude 5.5 family), read once.
        let catalogs = self.pi_catalogs.get_or_init(|| {
            let root = if cfg!(test) { None } else { agent::runtime::Runtime::installed().ok().map(|r| r.root) };
            provider::PROVIDERS.iter().map(|p| (p.id, root.as_ref().and_then(|r| provider::pi_catalog(r, p)))).collect()
        });
        let mut guard = self.state.lock().unwrap();
        if guard.ledger.is_none() { guard.ledger = Some(Ledger::load(&self.data.join(SPEND_FILE))); }
        let st = &*guard;
        let reason = |m: &ApiModel| -> Option<String> {
            if let Some(r) = m.unsupported() { return Some(r); }
            let p = m.provider_info()?;
            // The menu's name for the provider, how it is reached included ("Connect Anthropic API").
            matches!(st.auth.get(p.id), Some(ui::ProviderAuth::Missing)).then(|| format!("Connect {}", ui::provider_title(p.id, p.name, p.subscription)))
        };
        let added = |provider: &str, id: &str| st.models.iter().any(|m| m.key() == (provider, id));
        let selected = st.selection.api().filter(|(p, id)| added(p, id)).map(|(p, id)| (p.to_owned(), id.to_owned()));
        let models = st.models.iter().map(|m| ui::ApiModelRow {
            provider: m.provider.clone(), id: m.id.clone(), name: m.name.clone(), added: true, unavailable: reason(m),
            detail: Some({
                let entry = catalogs.get(m.provider.as_str()).and_then(|c| c.as_ref()).and_then(|c| provider::pi_entry(c, &m.id));
                ui::ApiModelDetail { context_steps: m.context_steps(), context: m.budget(), price: price_line(m),
                    thinking_levels: m.thinking_levels(entry).into_iter().map(String::from).collect(), thinking: m.thinking_level(entry).into() }
            }),
        });
        let mut catalog = vec![];
        let mut problems = vec![];
        for p in provider::PROVIDERS {
            match st.catalog.get(p.id).map(|(_, r)| r) {
                Some(Ok(list)) => catalog.extend(list.iter().map(|m| ui::ApiModelRow {
                    provider: m.provider.clone(), id: m.id.clone(), name: m.name.clone(),
                    added: added(&m.provider, &m.id), unavailable: m.unsupported(), detail: None,
                })),
                // A provider waiting for its key says so on its own picker section.
                Some(Err(e)) if !st.fetching.contains(p.id) && !matches!(st.auth.get(p.id), Some(ui::ProviderAuth::Missing)) => problems.push(e.clone()),
                _ => {}
            }
        }
        ui::ApiProviders {
            selected,
            unchosen: !st.selection.is_chosen(),
            models: models.collect(),
            providers: provider::PROVIDERS.iter().map(|p| ui::ApiProviderRow {
                id: p.id.into(), name: p.name.into(), icon: p.icon.into(), console_url: p.console_url.into(),
                auth: st.auth.get(p.id).cloned().unwrap_or(ui::ProviderAuth::Checking),
                notice: st.login_errors.get(p.id).cloned(),
                sign_in: p.sign_in,
                // Anthropic is "just connect, replace key or disconnect", no monthly-credit choice.
                credit: None,
                spend: None,
                custom_url: p.custom_url,
                key_required: p.key_required,
                endpoint: provider::base_url(p),
                group: if p.subscription { ui::ProviderGroup::Subscription } else { ui::ProviderGroup::ApiKey },
                // ChatGPT's sign-in has no key to paste instead.
                key_fallback: p.sign_in && p.id != provider::CHATGPT,
            }).collect(),
            catalog,
            catalog_problem: (!problems.is_empty()).then(|| problems.join(" · ")),
            engine_added: false,
            relay_added: false,
            relay_on: false,
        }
    }

    pub fn publish(&self) {
        let ui = *self.ui.lock().unwrap();
        if let Some(ui) = ui { ui.send(ui::UiUpdate::ApiProviders(self.snapshot())); }
    }
}

/// Whether a credential works now, and how Pi should use it. A key is checked against the model list (401/403 =
/// rejected); a Console sign-in is refreshed when near expiry and checked against the Console's config (401 =
/// signed out), which also updates its OpenCode Go endpoint. The error is the reason the menu shows.
pub async fn check_credential(p: &'static ProviderInfo, credential: &provider::Credential, data: &std::path::Path, models_url: Option<&str>)
    -> Result<agent::runtime::PiAuth, String> {
    // [providers] A provider on the user's own URL is checked against that URL's model list.
    let custom = p.custom_url.then(|| provider::models_url_for(p)).flatten();
    if p.custom_url && custom.is_none() { return Err(provider::KeyMissing(p.name).to_string()); }
    let models_url = custom.as_deref().or(models_url);
    match credential {
        provider::Credential::Key(key) => {
            let saved = provider::workspace_id(p);
            match provider::check_key(p, key, saved.as_deref(), models_url).await {
                KeyCheck::Valid { workspace } => {
                    // A key stored without its workspace (or whose workspace changed) keeps the one that worked.
                    if workspace != saved && provider::api_key(p).is_ok_and(|k| k == *key) {
                        if let Err(e) = provider::save_workspace(p, workspace.as_deref()) { eprintln!("{} workspace not saved: {e:#}", p.name); }
                    }
                    let api_key = agent::runtime::pi_api_key(p, credential, data, provider::is_installed_app()).map_err(|e| e.to_string())?;
                    Ok(agent::runtime::PiAuth::Key { workspace, api_key })
                }
                KeyCheck::Rejected => Err(provider::rejected_message(p)),
                KeyCheck::Refused(why) => Err(why),
                KeyCheck::Unreachable => Err(provider::unreachable_message(p)),
            }
        }
        provider::Credential::Console(_) => {
            // Signed out and SSO required are named and wait for a new sign-in; anything else is unreachable
            // (retried once a minute by the agent).
            let named = |e: &anyhow::Error| agent::console::is_signed_out(e) || e.downcast_ref::<agent::chatgpt::Refused>().is_some()
                || e.downcast_ref::<agent::console::SsoRequired>().is_some() || e.downcast_ref::<provider::KeyMissing>().is_some();
            let reason = |e: anyhow::Error| if named(&e) { e.to_string() } else { provider::unreachable_message(p) };
            let mut session = agent::console::access_token(p, &data.join("agent"), agent::console::Budget::APP).await.map_err(reason)?;
            // [providers] ChatGPT: the account's model list answers the token (401: signed out); no Console config.
            let chatgpt = p.id == provider::CHATGPT;
            if chatgpt {
                let api = models_url.and_then(|u| u.strip_suffix("/models")).unwrap_or(agent::chatgpt::API_BASE);
                // Signed out is named and waits for a sign-in; anything else (a 503, a slow network) is retried.
                agent::chatgpt::models(api, &session.access).await.map_err(|e| if agent::console::is_signed_out(&e) { e.to_string() } else { provider::unreachable_message(p) })?;
            }
            match if chatgpt { Ok(None) } else { agent::console::fetch_config(&session).await } {
                Ok(Some(config)) => {
                    let before = session.clone();
                    agent::console::apply_config(&mut session, p.id, &config);
                    if session != before { let _ = provider::save_console_session(p, &session); }
                }
                Ok(None) => {}
                Err(e) => return Err(reason(e)),
            }
            // The installed app's Pi asks this app's binary for a current token on every request (agent::runtime::pi_api_key).
            let api_key = agent::runtime::pi_api_key(p, credential, data, provider::is_installed_app()).map_err(|e| e.to_string())?;
            Ok(agent::runtime::PiAuth::Console { api_key, base_url: session.base_url.clone(), headers: session.request_headers(), models: session.models.clone() })
        }
    }
}

/// Whether a catalog load for `provider` changes any added row (`ApiModel::refreshed`). Only the ChatGPT sign-in: its
/// account list is the source of the window, images and efforts; the other providers' rows keep what was captured (an
/// Anthropic listing without Pi's catalog knows less than the pre-added Claude rows).
fn refreshes(added: &[ApiModel], provider: &str, catalog: &[ApiModel]) -> bool {
    provider == provider::CHATGPT && added.iter().filter(|m| m.provider == provider)
        .any(|m| catalog.iter().find(|c| c.id == m.id).is_some_and(|fresh| m.refreshed(fresh) != *m))
}

/// "$4 in · $20 out per M tokens" from the model's price.
fn price_line(m: &ApiModel) -> Option<String> {
    // Plan usage has no per-token price; Pi's catalog carries the API's.
    if m.provider == provider::CHATGPT { return None; }
    let cost = m.cost.as_ref()?;
    let dollars = |v: f64| if v >= 1.0 && v.fract() == 0.0 { format!("${v:.0}") } else { format!("${v:.2}") };
    let (input, output) = (cost["input"].as_f64()?, cost["output"].as_f64()?);
    (input > 0.0 || output > 0.0).then(|| format!("{} in · {} out per M tokens", dollars(input), dollars(output)))
}

/// A probe refusal that retrying cannot change: the plan cannot share its usage, the usage limit, a refused grant or a
/// dead sign-in (named on the provider); anything else is temporary.
fn is_plan_refusal(e: &anyhow::Error) -> bool {
    let t = e.to_string();
    agent::console::is_signed_out(e) || e.downcast_ref::<agent::chatgpt::Refused>().is_some()
        || t.contains("can't share its usage") || t.contains("usage limit reached") || t.contains("isn't available on the plan") || t.contains("not allowed")
}

/// Where the spend estimate keeps its daily totals (no request content, only dollars per day).
const SPEND_FILE: &str = "agent/api-spend.json";
/// One JSON line per background-job run: time, job, model, prompt hash, tokens, dollars, outcome.
const BACKGROUND_RUNS: &str = "agent/background-runs.jsonl";
/// Days of totals kept: enough for any monthly cycle and the seven-day pace.
const SPEND_DAYS_KEPT: i64 = 62;

/// Spend per provider per local calendar day, in US dollars. Anthropic's Messages API reports token counts, not
/// money, and no endpoint a normal API key can call reports the credit balance (the usage and cost reports need an
/// Admin key), so the balance is estimated here from each response's usage at the model's price.
#[derive(Default, Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Ledger {
    /// provider → local day number (days since 1970-01-01 in local time) → dollars.
    days: BTreeMap<String, BTreeMap<i64, f64>>,
}
impl Ledger {
    fn load(path: &std::path::Path) -> Self {
        std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self, path: &std::path::Path) -> anyhow::Result<()> {
        agent::runtime::atomic_write(path, &serde_json::to_vec(self)?)
    }
    fn add(&mut self, provider: &str, day: i64, usd: f64) {
        let days = self.days.entry(provider.into()).or_default();
        *days.entry(day).or_default() += usd;
        days.retain(|d, _| *d > day - SPEND_DAYS_KEPT);
    }
}

/// The local calendar date of `ms`: (day number since 1970-01-01, year, month 1-12, day of month).
fn local_date(ms: u64) -> (i64, i32, u32, u32) {
    let secs = (ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    let day = (secs as i64 + tm.tm_gmtoff as i64).div_euclid(86_400);
    (day, tm.tm_year + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32)
}
fn local_day(ms: u64) -> i64 {
    local_date(ms).0
}
fn spawn(f: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new().name("heimdall-api-providers".into()).spawn(f);
}

/// Every request inside carries its own deadline (agent::provider's client: 10 s connect, 20-45 s total).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime").block_on(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::HeaderMap, routing::get, Json, Router};
    use serde_json::json;

    async fn fake() -> (Urls, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/v1/models", get(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    None | Some("Bearer sk-good-0123456789abcdefghij") => Ok(Json(json!({"data":[{"id":"claude-haiku-5-5"},{"id":"beam-free"},{"id":"gemini-4-argon-contributor"}]}))),
                    _ => Err(axum::http::StatusCode::UNAUTHORIZED),
                }
            }))
            .route("/api.json", get(|| async { Json(json!({"opencode-go":{"npm":"@ai-sdk/openai-compatible","models":{
                "claude-haiku-5-5":{"name":"Claude Haiku 5.5","provider":{"npm":"@ai-sdk/anthropic"},"limit":{"context":1000000,"output":128000},"cost":{"input":0.1,"output":0.5,"tiers":[{"input":0.5,"output":2.5,"tier":{"type":"context","size":100000}}]}},
                "beam-free":{"name":"Beam Free","limit":{"context":1048576,"output":131072}},
                "gemini-4-argon-contributor":{"name":"Gemini 4 Argon Contributor","provider":{"npm":"@ai-sdk/google"},"limit":{"context":1048576,"output":65536}}}}})) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (Urls { models: Some(format!("http://{addr}/v1/models")), metadata: Some(format!("http://{addr}/api.json")), console: None, chatgpt: None }, task)
    }

    fn wait(mut done: impl FnMut() -> bool) {
        let until = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < until, "timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Default selection without a key, catalog from the live list plus metadata, connect (bad, then good key),
    /// the agent told once, the row available, then disconnect. Lab keys stay in memory; no Keychain is touched.
    #[test]
    // Starts from the personal edition's pre-selected model and pre-added rows; the public edition starts blank
    // (edition_blank_start_is_public_only).
    #[cfg(not(feature = "public"))]
    fn connect_catalog_and_rows_against_a_fake_provider() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (urls, task) = rt.block_on(fake());
        let dir = tempfile::tempdir().unwrap();
        let p = provider::provider(provider::OPENCODE_GO).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        provider::forget_key(p).unwrap();
        let hub = Hub::new(None, dir.path().into(), urls);
        let told = Arc::new(Mutex::new(vec![]));
        let t = told.clone();
        hub.on_key_changed(move |id| t.lock().unwrap().push(id));
        hub.set_settings(&crate::settings::Settings::default());
        hub.refresh(true);
        // The fake also answers for OpenRouter (its list is public); only OpenCode Go's rows are this test's.
        let go_rows = |s: &ui::ApiProviders| s.catalog.iter().filter(|r| r.provider == "opencode-go").cloned().collect::<Vec<_>>();
        wait(|| go_rows(&hub.snapshot()).len() == 3 && hub.auth(p.id) == Some(ui::ProviderAuth::Missing));
        let s = hub.snapshot();
        assert_eq!(s.selected, Some(("opencode-go".into(), "claude-haiku-5-5".into())));
        assert_eq!(s.models[0].unavailable.as_deref(), Some("Connect OpenCode Go subscription"));
        let detail = s.models[0].detail.clone().unwrap();
        assert_eq!((detail.context, detail.context_steps.len(), detail.price.as_deref()), (1_000_000, 8, Some("$0.10 in · $0.50 out per M tokens")), "the whole window until the slider moves");
        // The Claude models wait for Anthropic's key; its picker section says so instead of an error.
        assert_eq!(s.models.iter().filter(|m| m.provider == "anthropic").map(|m| m.unavailable.as_deref()).collect::<Vec<_>>(), [Some("Connect Anthropic API"); 3]);
        assert_eq!(s.catalog_problem, None);
        assert_eq!(go_rows(&s).iter().map(|r| (r.id.as_str(), r.added, r.unavailable.as_deref())).collect::<Vec<_>>(),
            [("claude-haiku-5-5", true, None), ("beam-free", false, None), ("gemini-4-argon-contributor", false, Some("Google endpoint not supported by Pi"))]);
        assert_eq!(hub.catalog_model("opencode-go", "beam-free").unwrap().api, provider::Api::OpenaiCompletions);
        // models.dev was cached for the next launch, when it was needed: Pi 1.4.0's own catalog describes these models,
        // so with that runtime built no metadata is fetched (night check 27 Oct: the assertion failed once the runtime was).
        let listed: Vec<serde_json::Value> = ["claude-haiku-5-5", "beam-free", "gemini-4-argon-contributor"].map(|id| json!({"id": id})).into();
        let pi = agent::runtime::Runtime::installed().ok().and_then(|r| provider::pi_catalog(&r.root, p));
        assert_eq!(dir.path().join("agent/models-dev-opencode-go.json").is_file(), provider::needs_metadata(&listed, pi.as_ref()));
        assert_eq!(hub.connect_key_now("opencode-go", "sk-bad-0123456789abcdefghijk"), Err("OpenCode Go: access rejected / no subscription".into()));
        assert!(told.lock().unwrap().is_empty() && !provider::has_credential(p));
        assert_eq!(hub.connect_key_now("opencode-go", " sk-good-0123456789abcdefghij "), Ok(()));
        assert_eq!(provider::api_key(p).unwrap(), "sk-good-0123456789abcdefghij");
        assert_eq!(*told.lock().unwrap(), ["opencode-go"]);
        let s = hub.snapshot();
        assert_eq!((s.models[0].unavailable.as_ref(), &s.provider("opencode-go").unwrap().auth), (None, &ui::ProviderAuth::Connected));
        // A runtime rejection shows on the provider; a later success clears it.
        hub.runtime_problem("opencode-go", Some("OpenCode Go: access rejected / no subscription".into()));
        assert!(matches!(hub.auth(p.id), Some(ui::ProviderAuth::Problem(_))));
        hub.runtime_problem("opencode-go", None);
        assert_eq!(hub.auth(p.id), Some(ui::ProviderAuth::Connected));
        hub.disconnect("opencode-go").unwrap();
        assert!(!provider::has_credential(p));
        assert_eq!(hub.snapshot().models[0].unavailable.as_deref(), Some("Connect OpenCode Go subscription"));
        assert_eq!(told.lock().unwrap().len(), 2);
        task.abort();
    }

    /// Audit 27 Oct, F2: a catalog load that knows more about an added row asks the app to refresh it; one that
    /// changes nothing, or describes another provider's rows, does not.
    #[test]
    fn a_catalog_with_newer_figures_refreshes_added_rows_only() {
        let sol = ApiModel { provider: provider::CHATGPT.into(), id: "gpt-6.1-sol".into(), context: 272_000, ..ApiModel::haiku() };
        let newer = ApiModel { context: 500_000, ..sol.clone() };
        assert!(refreshes(&[sol.clone()], provider::CHATGPT, &[newer.clone()]));
        assert!(!refreshes(&[newer.clone()], provider::CHATGPT, &[newer.clone()]));
        assert!(!refreshes(&[sol.clone()], provider::ANTHROPIC, &[newer]));
        assert!(!refreshes(&[sol], provider::CHATGPT, &[]));
        let opus = ApiModel::anthropic(provider::OPUS).unwrap();
        assert!(!refreshes(&[opus.clone()], provider::ANTHROPIC, &[ApiModel { reasoning: false, ..opus }]), "Anthropic rows keep what was captured");
    }

    /// Item 28: a background job's run joins Anthropic's day in the spend ledger (the agent's spend line counts it) and
    /// leaves one numbers-only line in `agent/background-runs.jsonl`.
    #[test]
    fn background_runs_count_in_the_spend_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(None, dir.path().into(), Urls::default());
        let usage = crate::background_call::Usage { input: 300, output: 2000, cache_read: 9000, cache_write_1h: 700, ..Default::default() };
        let at_ms = agent::now_ms();
        let run = crate::background_call::Run { job: "plan refresher", model: "claude-haiku-5-5".into(), hash: 0xabc, usage, usd: 0.00126, outcome: "answered", at_ms };
        hub.record_background(provider::ANTHROPIC, &run);
        hub.record_background(provider::ANTHROPIC, &crate::background_call::Run { usd: 0.002, ..run.clone() });
        let ledger = Ledger::load(&dir.path().join(SPEND_FILE));
        let day = ledger.days["anthropic"][&local_day(at_ms)];
        assert!((day - 0.00326).abs() < 1e-12, "{day}");
        let lines = std::fs::read_to_string(dir.path().join(BACKGROUND_RUNS)).unwrap();
        assert_eq!(lines.lines().count(), 2);
        let first: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!((first["job"].as_str(), first["cache_read"].as_u64(), first["prompt"].as_str()), (Some("plan refresher"), Some(9000), Some("0000000000000abc")));
        // A provider without a prepaid credit only logs the run.
        hub.record_background(provider::OPENCODE_GO, &run);
        assert!(!Ledger::load(&dir.path().join(SPEND_FILE)).days.contains_key("opencode-go"));
    }

    /// Anthropic end to end against a fake api.anthropic.com: the key goes in `x-api-key`, a rejected key is named, a
    /// first connection asks the app once to select Claude Opus 5.5 (a replaced key does not), the live list loads
    /// with the key, and each response's usage moves the spend line.
    #[test]
    #[cfg_attr(feature = "public", ignore = "the personal edition's pre-added models; public starts blank")]
    fn anthropic_key_selects_opus_once_lists_models_and_counts_spend() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p = provider::provider(provider::ANTHROPIC).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        provider::forget_key(p).unwrap();
        let app = Router::new().route("/v1/models", get(|h: HeaderMap| async move {
            match h.get("x-api-key").and_then(|v| v.to_str().ok()) {
                Some("sk-ant-api03-good-0123456789abcdefghij") => Ok(Json(json!({"data":[{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5","max_input_tokens":1000000,"max_tokens":128000},{"id":"claude-fable-5-1","display_name":"Claude Fable 5.1","max_input_tokens":1000000,"max_tokens":128000}]}))),
                _ => Err(axum::http::StatusCode::UNAUTHORIZED),
            }
        }));
        let (addr, task) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (addr, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
        });
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(None, dir.path().into(), Urls { models: Some(format!("http://{addr}/v1/models")), ..Default::default() });
        let (tx, rx) = crossbeam_channel::unbounded();
        hub.set_actions(tx);
        hub.set_settings(&crate::settings::Settings::default());
        assert_eq!(hub.connect_key_now("anthropic", "sk-ant-api03-bad-0123456789abcdefghij"), Err("Anthropic: key rejected".into()));
        assert!(rx.try_recv().is_err());
        assert_eq!(hub.connect_key_now("anthropic", "sk-ant-api03-good-0123456789abcdefghij"), Ok(()));
        assert_eq!(rx.try_recv().ok(), Some(ui::UiAction::AddProviderDefault { provider: "anthropic".into() }));
        assert_eq!(hub.connect_key_now("anthropic", "sk-ant-api03-good-0123456789abcdefghij"), Ok(()));
        assert!(rx.try_recv().is_err(), "a replaced key keeps User's selection");
        wait(|| hub.snapshot().catalog.iter().filter(|r| r.provider == "anthropic").count() == 2);
        let s = hub.snapshot();
        let row = |id: &str| s.catalog.iter().find(|r| r.provider == "anthropic" && r.id == id).unwrap().clone();
        assert!(row("claude-opus-5-5").added && !row("claude-fable-5-1").added);
        // Anthropic is connect / replace key / disconnect: no credit choice, no spend line.
        let anthropic = s.providers.iter().find(|p| p.id == "anthropic").unwrap();
        assert_eq!((anthropic.sign_in, anthropic.credit, anthropic.spend.as_deref(), anthropic.custom_url), (false, None, None, false));
        let opus = ApiModel::anthropic(provider::OPUS).unwrap();
        let u = Usage { input: 2_000, output: 500, cache_read: 40_000, cache_write: 3_000, cache_write_1h: 3_000 };
        hub.record_usage(&opus, &u, agent::now_ms());
        assert!(dir.path().join(SPEND_FILE).is_file());
        assert!(hub.snapshot().providers.iter().all(|p| p.spend.is_none()));
        hub.disconnect("anthropic").unwrap();
        task.abort();
    }

    /// The user's key (18 Oct) is not scoped to a workspace. Against a fake that answers like Anthropic (400 "must include
    /// the anthropic-workspace-id header" without it, the organisation's workspaces on /v1/organizations/workspaces),
    /// Connect keeps the key and the Default workspace, the picker's list names it, the agent's check hands it to Pi;
    /// a key stored without its workspace (by hand) finds and keeps it on the next check; a refusal shows Anthropic's
    /// own words in the panel.
    #[test]
    fn unscoped_anthropic_key_connects_through_its_default_workspace() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p = provider::provider(provider::ANTHROPIC).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        provider::forget_key(p).unwrap();
        const KEY: &str = "sk-ant-usr-lab-0123456789abcdefghij";
        let app = Router::new()
            .route("/v1/models", get(|h: HeaderMap| async move {
                let get = |n: &str| h.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
                match (get("x-api-key").as_deref(), get("anthropic-workspace-id").as_deref()) {
                    (Some(KEY), Some("wrkspc_default")) => Ok(Json(json!({"data":[{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5","max_input_tokens":1000000,"max_tokens":128000}]}))),
                    (Some(KEY), _) => Err((axum::http::StatusCode::BAD_REQUEST, r#"{"type":"error","error":{"type":"invalid_request_error","message":"This API key is not scoped to a workspace, so this request must include the anthropic-workspace-id header"}}"#)),
                    (Some("sk-ant-api03-disabled-0123456789abcdefghij"), _) => Err((axum::http::StatusCode::BAD_REQUEST, r#"{"type":"error","error":{"type":"invalid_request_error","message":"This organization has been disabled."}}"#)),
                    _ => Err((axum::http::StatusCode::UNAUTHORIZED, r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#)),
                }
            }))
            .route("/v1/organizations/workspaces", get(|| async { Json(json!({"data":[{"id":"wrkspc_default","name":"Default","archived_at":null}],"has_more":false})) }));
        let (addr, task) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (addr, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
        });
        let dir = tempfile::tempdir().unwrap();
        let models = format!("http://{addr}/v1/models?limit=1000");
        let hub = Hub::new(None, dir.path().into(), Urls { models: Some(models.clone()), ..Default::default() });
        hub.set_settings(&crate::settings::Settings::default());
        assert_eq!(hub.connect_key_now("anthropic", "sk-ant-api03-disabled-0123456789abcdefghij"), Err("Anthropic: This organization has been disabled.".into()));
        assert!(!provider::has_credential(p));
        assert_eq!(hub.connect_key_now("anthropic", KEY), Ok(()));
        assert_eq!((provider::api_key(p).unwrap().as_str(), provider::workspace_id(p).as_deref()), (KEY, Some("wrkspc_default")));
        wait(|| hub.snapshot().catalog.iter().any(|r| r.provider == "anthropic"));
        let auth = rt.block_on(check_credential(p, &provider::credential(p).unwrap(), dir.path(), Some(&models))).unwrap();
        assert_eq!(auth, agent::runtime::PiAuth::Key { workspace: Some("wrkspc_default".into()), api_key: "$HEIMDALL_ANTHROPIC_API_KEY".into() });
        // Stored by hand without the workspace: the picker's list finds it and keeps it.
        provider::save_workspace(p, None).unwrap();
        hub.state.lock().unwrap().catalog.remove(p.id);
        hub.load_catalog(p);
        assert_eq!(provider::workspace_id(p).as_deref(), Some("wrkspc_default"));
        assert!(hub.snapshot().catalog.iter().any(|r| r.provider == "anthropic" && r.id == "claude-opus-5-5"));
        // And the agent's check does the same.
        provider::save_workspace(p, None).unwrap();
        let auth = rt.block_on(check_credential(p, &provider::credential(p).unwrap(), dir.path(), Some(&models))).unwrap();
        assert_eq!(auth, agent::runtime::PiAuth::Key { workspace: Some("wrkspc_default".into()), api_key: "$HEIMDALL_ANTHROPIC_API_KEY".into() });
        assert_eq!(provider::workspace_id(p).as_deref(), Some("wrkspc_default"));
        hub.disconnect("anthropic").unwrap();
        assert_eq!(provider::workspace_id(p), None);
        task.abort();
    }

    // [providers] The two custom providers (OpenAI- and Anthropic-compatible) against fakes.

    async fn serve(app: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (addr, tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
    }

    /// An OpenAI-compatible server: Connect checks `<url>/models` with the key (a typed "/models" or trailing slash is
    /// trimmed), keeps URL and key, lists the server's models carrying the URL for Pi, and the agent's check reads
    /// the same URL; Disconnect forgets both. A local server connects without a key. An Anthropic-compatible server is
    /// listed at `<root>/v1/models` with `x-api-key`, and its models run on the Messages API at that root.
    #[test]
    fn custom_providers_connect_list_and_forget() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let custom = provider::provider(provider::CUSTOM).unwrap();
        let own = provider::provider(provider::CUSTOM_ANTHROPIC).unwrap();
        if std::env::var_os(custom.key_env).is_some() || std::env::var_os(own.key_env).is_some() { return; }
        provider::forget_key(custom).unwrap();
        provider::forget_key(own).unwrap();
        let app = Router::new()
            .route("/v1/models", get(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer sk-lab-0123456789abcdefghij") => Ok(Json(json!({"object":"list","data":[{"id":"llama-4.1-70b-instruct","object":"model","max_model_len":131072}]}))),
                    _ => Err(axum::http::StatusCode::UNAUTHORIZED),
                }
            }))
            // A local engine (LM Studio-like): no key, no window in its list.
            .route("/local/v1/models", get(|| async { Json(json!({"data":[{"id":"qwen3.8-32b-mlx"},{"id":"gemma-4-27b"}]})) }))
            // An Anthropic-compatible server: its own list, the key in x-api-key with anthropic-version.
            .route("/claude/v1/models", get(|h: HeaderMap| async move {
                match (h.get("x-api-key").and_then(|v| v.to_str().ok()), h.get("anthropic-version").is_some()) {
                    (Some("sk-lab-anthropic-0123456789abcdef"), true) => Ok(Json(json!({"data":[{"id":"claude-lab","display_name":"Claude Lab","max_input_tokens":200000,"max_tokens":32000}]}))),
                    _ => Err(axum::http::StatusCode::UNAUTHORIZED),
                }
            }));
        let (addr, task) = rt.block_on(serve(app));
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(None, dir.path().into(), Urls::default());
        hub.set_settings(&crate::settings::Settings::default());
        // Not connected: Missing, no list, the menu says Connect.
        hub.check_saved_key(custom);
        assert_eq!(hub.auth(custom.id), Some(ui::ProviderAuth::Missing));
        let base = format!("http://{addr}/v1");
        assert_eq!(hub.connect_endpoint_now("openai-compatible", "not a url", "sk-lab-0123456789abcdefghij"), Err("That is not a URL".into()));
        assert_eq!(hub.connect_endpoint_now("openai-compatible", &base, "sk-wrong-0123456789abcdefghij"), Err("OpenAI-Compatible: key rejected".into()));
        assert_eq!(hub.connect_endpoint_now("openai-compatible", "", ""), Err("Enter the server's URL".into()));
        assert!(!provider::has_credential(custom));
        assert_eq!(hub.connect_endpoint_now("openai-compatible", &format!("{base}/models/"), " sk-lab-0123456789abcdefghij "), Ok(()));
        assert_eq!((provider::base_url(custom), provider::api_key(custom).unwrap()), (Some(base.clone()), "sk-lab-0123456789abcdefghij".into()));
        wait(|| hub.catalog_model("openai-compatible", "llama-4.1-70b-instruct").is_some());
        let llama = hub.catalog_model("openai-compatible", "llama-4.1-70b-instruct").unwrap();
        assert_eq!((llama.api, llama.context, llama.base_url.as_deref(), llama.unsupported()), (provider::Api::OpenaiCompletions, 131_072, Some(base.as_str()), None));
        assert_eq!(llama.pi_definition().unwrap()["baseUrl"], base.as_str());
        let row = hub.snapshot().providers.into_iter().find(|p| p.id == "openai-compatible").unwrap();
        assert_eq!((row.custom_url, row.key_required, row.endpoint.as_deref(), row.auth), (true, false, Some(base.as_str()), ui::ProviderAuth::Connected));
        let auth = rt.block_on(check_credential(custom, &provider::credential(custom).unwrap(), dir.path(), None)).unwrap();
        assert_eq!(auth, agent::runtime::PiAuth::Key { workspace: None, api_key: "$HEIMDALL_CUSTOM_API_KEY".into() });
        hub.disconnect("openai-compatible").unwrap();
        assert_eq!((provider::base_url(custom), provider::has_credential(custom)), (None, false));
        // A local server: no key needed; its models default to a 128K window.
        let local = format!("http://{addr}/local/v1");
        assert_eq!(hub.connect_endpoint_now("openai-compatible", &local, ""), Ok(()));
        assert_eq!(provider::api_key(custom).unwrap(), provider::NO_KEY);
        wait(|| hub.catalog_model("openai-compatible", "gemma-4-27b").is_some());
        let gemma = hub.catalog_model("openai-compatible", "gemma-4-27b").unwrap();
        assert_eq!((gemma.context, gemma.base_url.as_deref(), gemma.unsupported()), (provider::MIN_CONTEXT, Some(local.as_str()), None));
        hub.disconnect("openai-compatible").unwrap();
        // Anthropic-compatible: a typed /v1 is dropped; a wrong key is refused; the list carries the root for Pi.
        let root = format!("http://{addr}/claude");
        assert_eq!(hub.connect_endpoint_now("anthropic-compatible", &root, "sk-wrong-0123456789abcdefghij"), Err("Anthropic-Compatible: key rejected".into()));
        assert_eq!(hub.connect_endpoint_now("anthropic-compatible", &format!("{root}/v1"), "sk-lab-anthropic-0123456789abcdef"), Ok(()));
        assert_eq!(provider::base_url(own), Some(root.clone()));
        wait(|| hub.catalog_model("anthropic-compatible", "claude-lab").is_some());
        let claude = hub.catalog_model("anthropic-compatible", "claude-lab").unwrap();
        assert_eq!((claude.api, claude.context, claude.base_url.as_deref()), (provider::Api::AnthropicMessages, 200_000, Some(root.as_str())));
        assert_eq!(claude.pi_definition().unwrap()["baseUrl"], root.as_str());
        assert!(rt.block_on(check_credential(own, &provider::credential(own).unwrap(), dir.path(), None)).is_ok());
        hub.disconnect("anthropic-compatible").unwrap();
        assert!(!provider::has_credential(own));
        task.abort();
    }

    /// The browser sign-in end to end against a fake Console: the session is kept (in memory here), the agent is
    /// told once, the provider shows Connected and the menu row can run; a cancelled sign-in keeps nothing.
    #[test]
    #[cfg_attr(feature = "public", ignore = "the personal edition's pre-added models; public starts blank")]
    fn browser_sign_in_connects_and_cancel_keeps_nothing() {
        use axum::routing::post;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p = provider::provider(provider::OPENCODE_GO).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        provider::forget_key(p).unwrap();
        let polls = Arc::new(Mutex::new(0u32));
        let n = polls.clone();
        let app = Router::new()
            .route("/console/auth/device/code", post(|| async { Json(json!({"device_code":"dev","user_code":"WXYZ-1234","verification_uri_complete":"/console/device?user_code=WXYZ-1234","expires_in":60,"interval":1})) }))
            .route("/console/auth/device/token", post(move || { let n = n.clone(); async move {
                *n.lock().unwrap() += 1;
                Json(json!({"access_token":"access","refresh_token":"refresh","expires_in":3600,"org_id":"org-1"}))
            }}))
            .route("/console/api/orgs", get(|| async { Json(json!([{"id":"org-1","name":"Personal"}])) }))
            .route("/console/api/v2/config", get(|| async { Json(json!({"providers":{}})) }));
        let (console, task) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (format!("http://{addr}/console"), tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
        });
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(None, dir.path().into(), Urls { models: Some("http://127.0.0.1:9/v1/models".into()), metadata: None, console: Some(console), chatgpt: None });
        let told = Arc::new(Mutex::new(0));
        let t = told.clone();
        hub.on_key_changed(move |_| *t.lock().unwrap() += 1);
        hub.set_settings(&crate::settings::Settings::default());
        hub.start_login("opencode-go".into());
        hub.cancel_login("opencode-go");
        std::thread::sleep(Duration::from_millis(1500));
        assert!(provider::console_session(p).is_none() && *told.lock().unwrap() == 0, "a cancelled sign-in keeps nothing");
        hub.start_login("opencode-go".into());
        wait(|| hub.auth(p.id) == Some(ui::ProviderAuth::Connected));
        let session = provider::console_session(p).unwrap();
        assert_eq!((session.access.as_str(), session.org_id.as_deref()), ("access", Some("org-1")));
        assert_eq!(*told.lock().unwrap(), 1);
        assert_eq!(hub.snapshot().models[0].unavailable, None);
        // The menu's check of a saved sign-in reads the Console config, not the key-only model list.
        hub.check_saved_key(p);
        assert_eq!(hub.auth(p.id), Some(ui::ProviderAuth::Connected));
        hub.disconnect("opencode-go").unwrap();
        assert!(provider::console_session(p).is_none());
        task.abort();
    }

    /// [providers] Sign in with ChatGPT end to end against a fake OpenAI: the consent page, the loopback callback, the
    /// kept session (with its issued client id), one test request on GPT-6 Luna, and no switch of the agent's model
    ///; then a plan that cannot share its usage is named; a cancelled sign-in keeps nothing.
    #[test]
    fn chatgpt_sign_in_probe_and_default_model_against_a_fake_openai() {
        use axum::routing::post;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p = provider::provider(provider::CHATGPT).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        provider::forget_key(p).unwrap();
        let eligible = Arc::new(Mutex::new(true));
        let e = eligible.clone();
        let tested = Arc::new(Mutex::new(Vec::<String>::new()));
        let t = tested.clone();
        let app = Router::new()
            .route("/api/accounts/oauth/token", post(|| async { Json(json!({"access_token":"chatgpt-access","refresh_token":"r","expires_in":3600,"scope":"openid offline_access resource.invoke chatgpt.tokens.use.direct","id_token":"x.y.z"})) }))
            .route("/v1/models", get(|h: HeaderMap| async move {
                if h["authorization"] != "Bearer chatgpt-access" { return Err(axum::http::StatusCode::UNAUTHORIZED); }
                Ok(Json(json!({"models":[{"slug":"gpt-6.1-sol","display_name":"GPT-6.1 Sol","visibility":"list"},{"slug":"gpt-6-luna","display_name":"GPT-6 Luna","visibility":"list"}]})))
            }))
            .route("/v1/responses", post(move |Json(b): Json<serde_json::Value>| { let (e, t) = (e.clone(), t.clone()); async move {
                t.lock().unwrap().push(b["model"].as_str().unwrap_or_default().to_owned());
                if *e.lock().unwrap() { (axum::http::StatusCode::OK, "data: {\"type\":\"response.completed\"}\n\n".to_string()) }
                else { (axum::http::StatusCode::FORBIDDEN, json!({"error":{"code":"subscription_sharing_user_not_eligible"}}).to_string()) }
            }}));
        let (base, task) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (format!("http://{addr}"), tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
        });
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(None, dir.path().into(), Urls { chatgpt: Some((base, 0)), ..Default::default() });
        let (tx, rx) = crossbeam_channel::unbounded();
        hub.set_actions(tx);
        hub.set_settings(&crate::settings::Settings::default());
        // The browser: the consent page's redirect, as a consenting user's browser follows it.
        let consent = |hub: &Arc<Hub>, issued: &str, registers: bool| {
            let mut url = String::new();
            wait(|| { url = hub.state.lock().unwrap().login_urls.get(p.id).cloned().unwrap_or_default(); !url.is_empty() });
            let q: BTreeMap<String, String> = reqwest::Url::parse(&url).unwrap().query_pairs().into_owned().collect();
            if registers {
                assert_eq!((q["client_id"].as_str(), q["agent_name_hint"].as_str()), ("dynamic_agent_client", "Heimdall"));
            } else {
                // Disconnect keeps the issued id (not secret): Connect again reuses Heimdall's one app entry in ChatGPT.
                assert!(q["client_id"] == issued && !q.contains_key("agent_name_hint"), "{q:?}");
            }
            let back = format!("{}?code=c&state={}&client_id={issued}", q["redirect_uri"], q["state"]);
            assert_eq!(reqwest::blocking::get(back).unwrap().status(), 200);
        };
        hub.start_login(p.id.into());
        hub.cancel_login(p.id);
        std::thread::sleep(Duration::from_millis(600));
        assert!(provider::console_session(p).is_none(), "a cancelled sign-in keeps nothing");
        hub.start_login(p.id.into());
        consent(&hub, "issued-1", true);
        wait(|| hub.auth(p.id) == Some(ui::ProviderAuth::Connected));
        let session = provider::console_session(p).unwrap();
        assert_eq!((session.access.as_str(), session.client_id.as_deref()), ("chatgpt-access", Some("issued-1")));
        assert_eq!(*tested.lock().unwrap(), vec![agent::chatgpt::TEST_MODEL.to_string()], "one test request, on the small model");
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err(), "signing in never switches the agent's model");
        let snap = hub.snapshot();
        let row = snap.providers.iter().find(|r| r.id == p.id).unwrap();
        assert_eq!((row.group, row.key_fallback, row.sign_in, row.name.as_str()), (ui::ProviderGroup::Subscription, false, true, "ChatGPT"));
        // The saved sign-in is checked against the model list.
        hub.check_saved_key(p);
        assert_eq!(hub.auth(p.id), Some(ui::ProviderAuth::Connected));
        // A plan that cannot share its usage: named, nothing selected.
        hub.disconnect(p.id).unwrap();
        *eligible.lock().unwrap() = false;
        hub.start_login(p.id.into());
        consent(&hub, "issued-1", false);
        wait(|| matches!(hub.auth(p.id), Some(ui::ProviderAuth::Problem(_))));
        assert_eq!(hub.auth(p.id), Some(ui::ProviderAuth::Problem("ChatGPT: this plan can't share its usage with apps · OpenAI allows Plus and Pro".into())));
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "nothing selected on an ineligible plan");
        hub.disconnect(p.id).unwrap();
        task.abort();
    }

    /// A failed sign-in reaches the menu (the panel may be gone), and a Console 403 is the SSO reason, not
    /// "unreachable" (which the agent would retry every minute).
    #[test]
    fn failed_sign_in_and_sso_are_named_for_the_menu() {
        use axum::routing::post;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p = provider::provider(provider::OPENCODE_GO).unwrap();
        if std::env::var_os(p.key_env).is_some() { return; }
        provider::forget_key(p).unwrap();
        let app = Router::new()
            .route("/console/auth/device/code", post(|| async { Json(json!({"device_code":"dev","user_code":"WXYZ-1234","verification_uri_complete":"/console/device","expires_in":60,"interval":1})) }))
            .route("/console/auth/device/token", post(|| async { (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":"expired_token"}))) }))
            .route("/console/api/v2/config", get(|| async { axum::http::StatusCode::FORBIDDEN }));
        let (console, task) = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            (format!("http://{addr}/console"), tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }))
        });
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::new(None, dir.path().into(), Urls { console: Some(console.clone()), ..Default::default() });
        hub.set_settings(&crate::settings::Settings::default());
        hub.start_login("opencode-go".into());
        wait(|| hub.snapshot().provider("opencode-go").unwrap().notice.is_some());
        assert_eq!(hub.snapshot().provider("opencode-go").unwrap().notice.as_deref(), Some("Sign-in failed: The sign-in code expired"));
        assert!(provider::console_session(p).is_none());
        let session = agent::console::Session { access: "a".into(), refresh: "r".into(), expires_ms: agent::now_ms() + 3_600_000, org_id: None, server: console, base_url: None, headers: Default::default(), models: vec![], client_id: None };
        provider::save_console_session(p, &session).unwrap();
        let e = rt.block_on(check_credential(p, &provider::Credential::Console(session), dir.path(), None)).unwrap_err();
        provider::forget_key(p).unwrap();
        assert_eq!(e, "OpenCode Go: sign in with your organization's SSO in the Console");
        task.abort();
    }
}
