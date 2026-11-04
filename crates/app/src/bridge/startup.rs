//! Opening the agent: active session, pending reloads, catch-up of unheard speech, idle compaction, process
//! ownership, provider check and `open`.
use super::*;

pub(super) fn active_session(data: &Path) -> Result<PathBuf> {
    let pointer = data.join("agent/active-session.json");
    if !pointer.exists() {
        return Ok(data.join("pi-session.jsonl"));
    }
    let path: PathBuf = serde_json::from_slice(&std::fs::read(pointer)?)?;
    anyhow::ensure!(
        path.starts_with(data)
            && !path
                .components()
                .any(|p| matches!(p, std::path::Component::ParentDir)),
        "agent session pointer outside data directory"
    );
    Ok(path)
}
pub(super) async fn engine_available(signal:&AtomicU8,probe:&agent::supervision::EngineProbe)->bool {
    // Ready is reported asynchronously: it can still describe the engine
    // before Off. Confirm readiness at the endpoint, including before a retry.
    match signal.load(Ordering::Acquire) {
        0 | 2 => probe.ready().await && signal.load(Ordering::Acquire) != 1,
        _ => false,
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PendingReload { #[serde(default)] pub(super) wait_for_settlement: bool, pub(super) reason: String, pub(super) generation: u64, #[serde(default)] pub(super) receipts: Vec<String> }
impl PendingReload {
    pub(super) fn accepted(reason: String, generation: u64) -> Self {
        let receipts=reason.split("; also requested ").filter_map(|r|{
            let id=r.split(':').next()?.trim();
            id.strip_prefix("reload-").and_then(|suffix|uuid::Uuid::parse_str(suffix).ok()).map(|_|id.to_owned())
        }).collect();
        Self {reason,generation,receipts,wait_for_settlement:true}
    }
}
pub(super) fn pending_reload(data: &Path, gate: &agent::delivery::DeliveryGate) -> Result<Option<PendingReload>> {
    let path = data.join("agent/pending-reload.json");
    if !path.exists() { return Ok(None); }
    let pending: PendingReload = serde_json::from_slice(&std::fs::read(&path)?)?;
    if pending.generation != gate.generation() { std::fs::remove_file(path)?; return Ok(None); }
    Ok(Some(pending))
}
/// Speech from the archived timeline between `since_ms` (`Bridge::catchup_since`: after the conversation's newest
/// message or the start of its last compaction, at most 30 minutes back) and a start (`on_ms`) that the conversation
/// never received: every utterance the pipeline recorded (all speakers, the agent's `[Mic · User]` labels, the clock
/// time it was heard) that the live spool does not hold. The archive persists, so speech heard before an app relaunch
/// counts too. Rows the pipeline routed away from the agent (`given_to_agent: false`: archive-only sources,
/// undiarized call streams, unavailable audio) are never handed over. Spool rows that never reached the model are
/// added by `Bridge::begin_start` (E4.4).
pub(super) fn unheard_speech(archive_root: &Path, spool: &agent::Bridge<RecordedAdapter>, since_ms: u64, on_ms: u64) -> Result<Vec<agent::notice::CatchupLine>> {
    let db = heimdall_archive::store::open(archive_root)?;
    let mut stmt = db.prepare("SELECT id,coalesce(source_id,''),coalesce(source,''),coalesce(speaker,''),coalesce(text,''),wall_ms FROM utterances WHERE wall_ms>=?1 AND wall_ms<?2
        AND (NOT json_valid(decision) OR coalesce(json_extract(decision,'$.given_to_agent'),1)!=0) ORDER BY wall_ms,id")?;
    let rows = stmt.query_map([since_ms as i64, on_ms as i64], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?, r.get::<_,String>(2)?, r.get::<_,String>(3)?, r.get::<_,String>(4)?, r.get::<_,i64>(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut lines = vec![];
    for (id, source_id, source, speaker, text, wall_ms) in rows {
        // A speech-split child (`<id>:speech:N`) re-labels part of its parent observation: heard if the parent was.
        let observation = id.split(":speech:").next().unwrap_or(&id);
        if text.trim().is_empty() || spool.has_observation(observation)? { continue; }
        // The same label the pipeline gives the agent: a microphone is "Mic", any other source its name.
        let source = if source_id.starts_with("mic:") { "Mic".to_owned() } else { source };
        let o = Observation { id: id.clone(), timestamp_ms: 0, source, speaker, language: String::new(), text };
        lines.push(agent::notice::CatchupLine { id, at_ms: Some(wall_ms.max(0) as u64), text: o.labelled(), observation: true });
    }
    Ok(lines)
}

/// The launch time an archive session's name records (`session-<unix ms>-<uuid>`).
pub(super) fn launch_ms(session: &Path) -> Option<u64> {
    session.file_name()?.to_str()?.strip_prefix("session-")?.split('-').next()?.parse().ok()
}
/// The start context of a fresh start (E4.1): the archived speech before `on_ms` the conversation never received,
/// from where its history ends (`Bridge::catchup_since`); `Bridge::deliver_start_context` adds the spool leftovers,
/// the reason and the restart message. A context that failed on the previous start (`carried`) is tried once more
/// here.
pub(super) fn start_context(b: &agent::Bridge<RecordedAdapter>, session: &Path, on_ms: u64, carried: &mut Option<agent::notice::StartContext>) -> Option<agent::notice::StartContext> {
    if !b.start_pending() { return None; }
    let mut ctx = agent::notice::StartContext::new(b.clock().now_ms(), None);
    let since = b.catchup_since(on_ms).unwrap_or_else(|e| { eprintln!("Agent catch-up from 30 minutes back: {e:#}"); on_ms.saturating_sub(agent::notice::CONTEXT_WINDOW_MS) });
    ctx.since_ms = Some(since);
    match unheard_speech(&crate::archive::store_root(session), b, since, on_ms) {
        Ok(lines) => ctx.add_lines(lines),
        Err(e) => eprintln!("Agent catch-up without archived speech: {e:#}"),
    }
    if let Some(old) = carried.take() {
        ctx.add_lines(old.lines);
        ctx.failed_on = old.failed_on;
    }
    Some(ctx)
}
/// The compaction threshold the start context is measured against (E4.2).
pub(super) fn start_threshold(settings: &Settings) -> impl Fn(u64) -> u64 + '_ {
    move |window| match settings.selected_api_model() {
        Some(model) => model.limits().threshold(),
        None => agent::provider::Limits::for_context(window.max(8192), None).threshold(),
    }
}
/// Adds the start context once the new Pi is idle (E4.1), compacting first when it would pass the threshold
/// (E4.2), and journals each step. A context that cannot be added is tried once more on the next start (a new Pi),
/// then dropped with a log line; wakes never wait on it after a failure.
#[allow(clippy::too_many_arguments)]
pub(super) async fn deliver_start(b: &mut agent::Bridge<RecordedAdapter>, start: &mut Option<agent::notice::StartContext>, carried: &mut Option<agent::notice::StartContext>, settings: &Settings, compaction_held: bool, data: &Path, gate: &Arc<agent::delivery::DeliveryGate>) -> Result<()> {
    let Some(ctx) = start.as_mut() else { return Ok(()) };
    let pid = match &b.harness.inner { Adapter::Pi(pi) => pi.pid(), _ => return Ok(()) };
    if pid.is_none() { return Ok(()); }
    // A failed idle compaction holds every compaction until a reply succeeds; the context is added as it is.
    if compaction_held { ctx.compacted = true; }
    let was_compacted = ctx.compacted;
    // Pi's own threshold this start runs with (a carried history raises it, `Limits::carrying`); the settings' otherwise.
    let in_turn = b.compaction_policy().map(|p| p.in_turn);
    let fallback = start_threshold(settings);
    match b.deliver_start_context(ctx, move |window| in_turn.unwrap_or_else(|| fallback(window))).await {
        Ok(true) => {
            let ids: Vec<String> = ctx.lines.iter().filter(|l| l.observation).map(|l| l.id.clone()).collect();
            b.harness.journal.event("agent_catchup", "agent:pi", &ids, capture::host_now_ns(), json!({"state":"added","compacted_first":ctx.compacted,"lines":ctx.lines.len(),"bytes":ctx.text().len(),"reason":ctx.reason}))?;
            *start = None;
            // The replacement answered: a reload this start carried out is complete (its `reload_me` receipts too).
            complete_pending_reload(data, gate)?;
        }
        Ok(false) => {
            if ctx.compacted && !was_compacted {
                b.harness.journal.event("agent_catchup", "agent:pi", &[], capture::host_now_ns(), json!({"state":"compacting_first","lines":ctx.lines.len()}))?;
            }
        }
        Err(e) => {
            let retry = ctx.failed_on.is_none();
            eprintln!("Agent catch-up not added{}: {e:#}", if retry { "; trying again on the next start" } else { "; dropped" });
            b.harness.journal.event("agent_catchup", "agent:pi", &[], capture::host_now_ns(), json!({"state":if retry{"retry_next_start"}else{"dropped"},"reason":format!("{e:#}")}))?;
            b.abandon_start_context();
            let mut failed = start.take().expect("start context");
            if retry { failed.failed_on = pid; failed.compacted = false; *carried = Some(failed); }
        }
    }
    Ok(())
}
/// A pending reload of this epoch is done once its replacement took the start context.
pub(super) fn complete_pending_reload(data: &Path, gate: &Arc<agent::delivery::DeliveryGate>) -> Result<()> {
    let Some(pending) = pending_reload(data, gate)? else { return Ok(()) };
    let permit = gate.permit()?;
    let actions = agent::actions::Actions::open(&data.join("agent/mcp-actions.sqlite"))?;
    for receipt in pending.receipts { actions.set_state(&receipt, "completed", &permit)?; }
    std::fs::remove_file(data.join("agent/pending-reload.json"))?;
    Ok(())
}
/// When Heimdall compacts on its own (`agent::maintenance`, night pass H1): Pi compacts by itself when a request
/// would pass the threshold, which can land in the middle of a reply (18 Oct: 18-20 s of silence at 13:35 and
/// mid-turn at 13:49), and the cold-cache compaction ran inside the user's first wake (19 Oct 18:12, 45 s). Heimdall
/// compacts while idle and quiet instead, once the history nears the threshold or the provider's hour cache for it
/// has gone cold; the user's input cancels it. The request budget is unchanged. App tests run the check without the
/// quiet wait (the simulator owns that rule).
pub(super) fn compaction_policy(settings: &Settings, window: u64, history: Option<u64>) -> agent::maintenance::CompactionPolicy {
    let (limits, hour_cache) = match settings.selected_api_model() {
        Some(model) => (model.limits().carrying(history), model.provider_info().is_some_and(|p| p.hour_cache)),
        None => (agent::provider::Limits::for_context(window.max(8192), None), false),
    };
    let mut policy = agent::maintenance::CompactionPolicy::for_limits(limits, hour_cache);
    if cfg!(test) { policy.quiet_ms = 0; }
    policy
}
/// Why the agent reloads once a carried history is summarised (the reload's line in the conversation).
pub(super) const CARRY_RELOAD: &str = "the history from the previous model is summarised, so compaction follows this model's budget again";
/// The policy after `event` when it ends a carry (`CompactionPolicy::carrying`): the first successful compaction of a
/// start that carried a history past its budget. Pi's raised in-turn threshold is in its settings for this process, so
/// the host reloads it (after the turn, conversation kept; the next start reads a compacted history and carries
/// nothing). `None`: nothing to do.
pub(super) fn carry_ended(policy: Option<agent::maintenance::CompactionPolicy>, event: &Value) -> Option<agent::maintenance::CompactionPolicy> {
    let policy = policy.filter(|p| p.carrying)?;
    let done = event["type"] == "compaction_end" && event["aborted"] != true && event["errorMessage"].is_null() && !event["result"].is_null();
    done.then_some(agent::maintenance::CompactionPolicy { carrying: false, ..policy })
}
/// The restart budget's clock. Tests move it forward (`test-restart-clock`, milliseconds) so restarts minutes apart
/// can be checked in seconds; the app reads the wall clock.
pub(super) fn budget_now(data: &Path) -> u64 {
    let offset = if cfg!(test) { std::fs::read_to_string(data.join("test-restart-clock")).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0) } else { 0 };
    agent::now_ms() + offset
}
/// One idle-maintenance step: `Bridge::maintain`, journalled when it starts a compaction.
pub(super) async fn maintain_idle(b: &mut agent::Bridge<RecordedAdapter>) -> Result<()> {
    if let Some(reason) = b.maintain().await? {
        b.harness.journal.event("agent_compaction", "agent:pi", &[], capture::host_now_ns(), json!({"state":"idle","reason":reason}))?;
    }
    Ok(())
}
/// The cache-refresh ping (`Bridge::keep_cache_warm`), journalled when one goes out. A failed ping is logged by the
/// caller; it never holds the idle compactions back, nor they it.
pub(super) async fn ping_idle(b: &mut agent::Bridge<RecordedAdapter>, gate: &agent::delivery::DeliveryGate) -> Result<()> {
    if let Some(id) = b.keep_cache_warm(background_calls_allowed(gate)).await? {
        b.harness.journal.event("agent_cache_ping", "agent:pi", &[], capture::host_now_ns(), json!({"id":id}))?;
    }
    Ok(())
}
/// Whether Heimdall may make a model call nobody asked for (the cache-refresh ping): only while the Agent toggle is
/// on. The one place this is decided, for the shared agent-off gate on background calls.
pub(super) fn background_calls_allowed(gate: &agent::delivery::DeliveryGate) -> bool {
    !gate.stopped_for(agent::delivery::StopReason::Agent) && gate.load(std::sync::atomic::Ordering::Acquire)
}
/// A pending reload left by an earlier supervisor (the app quit or the agent runtime stopped before its replacement
/// was verified) is not this start's reload: it is recorded and cleared, and no outcome note claims it.
/// Its `reload_me` receipts are completed, since this fresh start is the reload they asked for.
pub(super) fn settle_stale_reload(data: &Path, session: &Path, gate: &Arc<agent::delivery::DeliveryGate>) -> Vec<String> {
    let path = data.join("agent/pending-reload.json");
    let Ok(bytes) = std::fs::read(&path) else { return vec![] };
    let pending = serde_json::from_slice::<PendingReload>(&bytes).ok();
    let receipts = pending.as_ref().map(|p| p.receipts.clone()).unwrap_or_default();
    // Never a reason to keep the agent from starting: every step is best effort and logged.
    if let Err(e) = heimdall_archive::Journal::open(session).and_then(|j| j.event("agent_reload_stale", "agent:pi", &[], capture::host_now_ns(), json!({"reason":pending.map(|p|p.reason)}))) { eprintln!("Stale reload not journalled: {e:#}"); }
    if let Err(e) = std::fs::remove_file(&path) { eprintln!("Stale reload not cleared: {e:#}"); }
    complete_receipts(data, gate, receipts)
}
/// Marks `reload_me` receipts completed when a delivery permit is available (the agent is on); returns the ones
/// still waiting.
pub(super) fn complete_receipts(data: &Path, gate: &Arc<agent::delivery::DeliveryGate>, receipts: Vec<String>) -> Vec<String> {
    if receipts.is_empty() { return receipts; }
    let Ok(permit) = gate.permit() else { return receipts };
    let result = agent::actions::Actions::open(&data.join("agent/mcp-actions.sqlite"))
        .and_then(|actions| receipts.iter().try_for_each(|r| actions.set_state(r, "completed", &permit)));
    match result {
        Ok(()) => vec![],
        Err(e) => { eprintln!("Stale reload receipts not completed: {e:#}"); receipts }
    }
}
pub(super) static CLEANUP_NOTICE_SHOWN: AtomicBool = AtomicBool::new(false);
pub(super) fn record_process_ownership(
    journal: &heimdall_archive::Journal,
    limited: bool,
    notice_shown: &AtomicBool,
    mut notice: impl FnMut(ui::HealthCue),
) -> Result<()> {
    journal.event("agent_process_ownership", "agent:pi", &[], capture::host_now_ns(),
        json!({"cleanup_limited":limited,"mechanisms":if limited { "owner_token,parent_tree,birth_checked_signals" } else { "responsibility,owner_token,parent_tree,birth_checked_signals" }}))?;
    if limited && !notice_shown.swap(true, Ordering::AcqRel) {
        notice(ui::HealthCue::Notice { part: ui::Part::Agent, label: "Agent cleanup is limited".into() });
    }
    Ok(())
}

#[cfg(test)]
thread_local! { pub(super) static TEST_STARTUP_AFTER_AWAIT: std::cell::RefCell<Option<Box<dyn FnMut(&str)>>> = const { std::cell::RefCell::new(None) }; }
pub(super) async fn startup_step<T>(stage: &str, permit: &agent::delivery::DeliveryPermit, future: impl std::future::Future<Output=Result<T>>) -> Result<T> {
    let result = permit.run(future).await;
    #[cfg(test)]
    TEST_STARTUP_AFTER_AWAIT.with(|hook| { if let Some(hook)=&mut *hook.borrow_mut() { hook(stage); } });
    #[cfg(not(test))]
    let _ = stage;
    permit.check()?;
    result
}

/// The thinking level the agent's model runs (the menu's choice, clamped to the model): an API model's own, else the
/// local engine's for its model. `runtime_root` reads Pi's catalog for models outside the Claude 5.5 family.
pub(crate) fn agent_thinking(s: &Settings, runtime_root: Option<&Path>) -> &'static str {
    match s.selected_api_model() {
        Some(model) => {
            let catalog = runtime_root.zip(model.provider_info()).and_then(|(root, p)| agent::provider::pi_catalog(root, p));
            model.thinking_level(catalog.as_ref().and_then(|c| agent::provider::pi_entry(c, &model.id)))
        }
        None => s.engine_thinking_level(),
    }
}

/// Whether the selected API model can start now and how Pi authenticates: a Console sign-in or a pasted key that
/// its provider accepts. The error is the reason the menu and the agent window show ("Connect OpenCode Go",
/// "OpenCode Go: access rejected / no subscription", "OpenCode Go: sign in again").
pub(super) async fn check_provider(model: &agent::provider::ApiModel, data: &Path) -> std::result::Result<agent::runtime::PiAuth, String> {
    if let Some(reason) = model.unsupported() { return Err(format!("{}: {reason}", model.name)); }
    let provider = model.provider_info().ok_or("Provider not supported")?;
    let credential = agent::provider::credential(provider).map_err(|e| e.to_string())?;
    crate::api_providers::check_credential(provider, &credential, data, None).await
}

pub(super) async fn open(
    s: &Settings,
    data: &Path,
    session: &Path,
    server: Option<&agent::mcp::McpServer>,
    ui: Option<ui::Ui>,
    api_hub: &crate::api_providers::Hub,
    gate: Arc<agent::delivery::DeliveryGate>,
    engine_signal: Arc<AtomicU8>,
    reload_note: Option<String>,
    startup_permit: Option<agent::delivery::DeliveryPermit>,
) -> Result<agent::Bridge<RecordedAdapter>> {
    let startup = if s.dry_run { None } else { Some(match startup_permit { Some(p)=>p, None=>gate.permit()? }) };
    if let Some(p)=&startup { p.check()?; }
    let pending = if s.dry_run { None } else { pending_reload(data, &gate)? };
    // The outcome note is written only once this start is verified (below). A reload this supervisor began whose
    // first replacement failed completes here on an automatic restart and says so, once.
    let reload_note = reload_note.or_else(|| pending.as_ref().map(|p|reload_line(&p.reason)));
    let feed = AgentFeed::open(s, data, session, ui);
    let run = crate::archive::run_dir(data, session);
    let spool = if s.dry_run {
        crate::settings::private_dir(&run)?;
        run.join("dry-run-spool.sqlite")
    } else {
        data.join("pi-spool.sqlite")
    };
    // The (provider, model) this start asked Pi for; Pi must report the same one.
    let mut selected_model: Option<(String, String)> = None;
    // The context window this start runs with (the compaction policy's budget for the local engine).
    let mut pi_window: u64 = 0;
    // The conversation's size at its last request, for an API model's start (`Limits::carrying`).
    let mut pi_history: Option<u64> = None;
    let mut h = if s.dry_run {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut o = OpenOptions::new();
        o.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        Adapter::Dry(LoggedDry {
            feed: feed.clone(),
            file: o.open(run.join("dry-run.jsonl"))?,
            path: run.join("dry-run.jsonl"),
            session: session.to_owned(),
            tx,
            rx,
        })
    } else {
        let path = active_session(data)?;
        let p=startup.as_ref().unwrap();
        let fixture=fixture_executable();
        let runtime = startup_step("prepare", p, async {
            if fixture.is_some() { return Ok(None); }
            Ok(Some(tokio::task::spawn_blocking(agent::runtime::Runtime::verified).await??))
        }).await?;
        let engine=agent::supervision::EngineProbe::new(agent::runtime::BASE_URL)?;
        // An API model needs no local engine: its key and provider are checked instead, and a model that cannot
        // run yet leaves the agent waiting with the reason (no restart loop).
        let api = if fixture.is_some() { None } else { s.selected_api_model().cloned() };
        // No model chosen yet (the public edition's blank start): the agent waits for a provider instead of starting.
        if fixture.is_none() && !s.agent_model.is_chosen() {
            return Err(agent::harness::ProviderUnavailable(agent::provider::NO_MODEL_CHOSEN.into()).into());
        }
        let mut pi_auth = None;
        if let Some(model) = &api {
            let checked = startup_step("provider", p, async { Ok(check_provider(model, data).await) }).await?;
            if let Some(provider) = model.provider_info() {
                // A missing key is the menu's own state (Connect…); a rejected or unreachable one is a problem.
                if checked.as_ref().err().is_none_or(|m| *m != format!("Connect {}", provider.name)) {
                    api_hub.runtime_problem(provider.id, checked.as_ref().err().cloned());
                }
            }
            pi_auth = Some(checked.map_err(agent::harness::ProviderUnavailable)?);
        }
        let ready=startup_step("readiness",p,async { Ok(api.is_some() || fixture.is_some() || engine_available(&engine_signal,&engine).await) }).await?;
        if !ready { return Err(agent::harness::EngineUnavailable.into()); }
        let context=startup_step("metadata",p,async {
            // [menu lane] The local model runs inside the budget its slider chose (the whole window by default).
            if let Some(model)=&api { Ok(model.limits().context) } else if fixture.is_some() { Ok(102400) } else { engine.context_limit().await.map(|limit| s.engine_context_budget(limit)).map_err(|_| agent::harness::EngineUnavailable.into()) }
        }).await?;
        pi_window=context;
        let config=if let Some(exe)=fixture {
            let mut c=PiConfig::fixture(exe,path,data.to_owned());
            c.mcp=server.map(|s|(s.url.clone(),s.token.clone()));c
        } else {
            let runtime=runtime.context("runtime missing")?;
            let catalog=api.as_ref().and_then(|m|m.provider_info()).and_then(|p|agent::provider::pi_catalog(&runtime.root,p));
            let dir=match &api {
                Some(model)=>{
                    // Audit 27 Oct, F6: a history past this model's budget (a switch from a bigger one) that fits its
                    // window is summarised at the first quiet moment, not in front of the next request.
                    pi_history=agent::session::history_tokens(&path).unwrap_or_else(|e|{eprintln!("Agent: history size unread: {e:#}");None});
                    let (dir,limits)=agent::runtime::configure_api_carrying(data,model,catalog.as_ref(),pi_auth.as_ref().context("provider authentication")?,pi_history)?;
                    if limits.carry>0 {
                        eprintln!("Agent: the history ({} tokens) is past {}'s {}-token budget; it compacts at the first quiet moment (Pi's in-turn limit {} for this start, not {})",
                            pi_history.unwrap_or(0),model.name,limits.context,limits.pi_threshold(),limits.threshold());
                    }
                    dir
                }
                None=>agent::runtime::configure(data,context)?,
            };
            let server=server.context("MCP server unavailable")?;
            let mut cfg=PiConfig::owned(&runtime,data,dir,server.url.clone(),server.token.clone());
            cfg.thinking=agent_thinking(s,Some(&runtime.root)).into();
            if api.is_some() { cfg.engine=None; }
            cfg.api_model=api.clone();
            cfg.engine_signal=Some(engine_signal);cfg.session=path;
            cfg.max_text_bytes=(context as usize/2).min(8*1024*1024);cfg
        };
        let (provider, model) = config.selected_model();
        selected_model = Some((provider.to_owned(), model.to_owned()));
        startup.as_ref().unwrap().check()?;
        Adapter::Pi(Pi::launch_config_permitted(config, startup.clone())?)
    };
    let observation_ownership = Arc::new(Mutex::new(HashMap::new()));
    if !s.dry_run {
        let observation_ownership = observation_ownership.clone();
        let archive = session.to_owned();
        let feed = feed.clone();
        let recorder: agent::harness::TransportRecorder = Arc::new(move |direction, wire| {
            let journal = heimdall_archive::Journal::open(&archive)?;
            let mut stored = wire.clone();
            reference_transport_images(&journal, &mut stored)?;
            if direction == "sent" {
                feed.sent(&stored);
            } else if direction == "reply" {
                feed.received(&stored);
            }
            let mut ids = vec![];
            collect_transport_ids(wire, &observation_ownership.lock().unwrap(), &mut ids);
            ids.sort();
            ids.dedup();
            journal.event(
                &format!("agent_transport_{direction}"),
                "agent:pi",
                &ids,
                capture::host_now_ns(),
                json!({"wire":stored}),
            )
        });
        if let Adapter::Pi(h) = &mut h {
            record_process_ownership(&heimdall_archive::Journal::open(session)?, h.cleanup_limited(),
                &CLEANUP_NOTICE_SHOWN, |cue| { if let Some(ui)=ui { ui.send(ui::UiUpdate::Health(cue)); } })?;
            h.set_transport_recorder(recorder);
            // Gate and cancellation already installed before guardian admission.
        }
        if fixture_executable().is_none() {
            if let Adapter::Pi(h) = &mut h {
                let p=startup.as_ref().unwrap();
                let state = startup_step("state",p,h.state()).await?;
                let (provider, id) = selected_model.as_ref().context("selected model missing")?;
                let running = &state["model"];
                // Pi names its provider; an older reply without one is judged by the model id alone.
                anyhow::ensure!(
                    running["id"] == id.as_str() && running["provider"].as_str().is_none_or(|p| p == provider),
                    "Pi selected another model ({}/{} instead of {provider}/{id})",
                    running["provider"].as_str().unwrap_or("?"), running["id"].as_str().unwrap_or("?")
                );
                startup_step("bridge",p,h.query(json!({"type":"prompt","message":"/heimdall_bridge_status"}))).await?;
                let entries = startup_step("entries",p,h.incremental_entries()).await?;
                let status = entries["entries"]
                    .as_array()
                    .and_then(|a| {
                        a.iter()
                            .rev()
                            .find(|e| e["customType"] == "heimdall_bridge")
                    })
                    .context("Pi MCP bridge did not report ready")?;
                anyhow::ensure!(
                    status["data"]["connected"] == true
                        && status["data"]["protocol"] == "2025-06-18"
                        && status["data"]["toolCount"].as_u64().unwrap_or(0) > 0,
                    "Pi MCP bridge is not ready"
                );
            }
        }
    }
    let mut b = agent::Bridge::open(
        RecordedAdapter {
            inner: h,
            journal: heimdall_archive::Journal::open(session)?,
            gate: if s.dry_run { None } else { Some(gate.clone()) },
            ids: observation_ownership,
            queries: HashMap::new(),
        },
        spool,
    )?;
    if !s.dry_run {
        b.recover_receipts(&active_session(data)?)?;
    }
    // A verified start (E4): everything pending from before is history; the catch-up, the reload's reason and the
    // restart message enter the conversation before any wake, and no notice wakes the agent until the user speaks.
    // The reload stays pending until this replacement takes the start context (`complete_pending_reload`).
    if matches!(b.harness.inner, Adapter::Pi(_)) {
        b.begin_start(reload_note)?;
        b.set_compaction_policy(Some(compaction_policy(s, pi_window, pi_history)));
    }
    if let Some(p)=&startup { p.check()?; }
    Ok(b)
}

/// The line before the restart message after a reload (E4.1): `Reloaded: <reason>` without receipt ids.
pub(super) fn reload_line(reason: &str) -> String {
    format!("Reloaded: {}.", reload_context(reason).reason.trim_end_matches('.'))
}
/// The island's health key for a wake that failed and is being retried (bug notes 26 Oct 00:52:29, 00:54:39).
pub(super) const RETRY_KEY: &str = "agent-retry";
/// What the island says for one resolution: `retry_in_ms` is how long the resend still waits, when it waits. A
/// passing failure says "retrying in N s" and, once its resends are spent, "gave up, say it again".
pub(super) fn resolution_cues(r: &agent::Resolution, retry_in_ms: Option<u64>) -> Vec<ui::HealthCue> {
    let notice = |label: String| ui::HealthCue::Notice { part: ui::Part::Agent, label };
    let retrying = |label: String| ui::HealthCue::Restarting { key: RETRY_KEY.into(), part: ui::Part::Agent, label };
    let clear = || ui::HealthCue::Clear { key: RETRY_KEY.into() };
    match r.outcome {
        // A resend at once is silent: the next arrow or answer shows it.
        agent::Resolved::Retrying => match retry_in_ms {
            Some(ms) => vec![retrying(format!("Model not answering · retrying in {} s", ms.div_ceil(1000).max(1)))],
            None => vec![],
        },
        agent::Resolved::Resent => vec![retrying("Model not answering · retrying now".into())],
        // A Stop is the user's own (the loop guard shows its own notice).
        agent::Resolved::Stopped => vec![clear()],
        // A restart turned it into history (E4.4), or context could not be added: nothing for the user to do.
        agent::Resolved::Failed if r.reason.starts_with("interrupted by a restart") || r.reason.starts_with("context not added") => vec![],
        agent::Resolved::Failed => vec![ui::HealthCue::Problem {
            key: RETRY_KEY.into(),
            part: ui::Part::Agent,
            label: if agent::notice::transient_failure(&r.reason) { "Model not answering · gave up" } else { "Agent couldn't answer · gave up" }.into(),
            action: "Say it again".into(),
            control: ui::HealthControl::OpenAgent,
        }],
        // The provider's refusal in plain words (E10.2).
        agent::Resolved::Rejected => vec![clear(), notice(r.reason.clone())],
        agent::Resolved::SettledAfterError => vec![clear(), notice("Agent stopped partway after an error".into())],
    }
}
/// What the island shows under `RETRY_KEY`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum RetryCue { #[default] None, Retrying, GaveUp }
/// A failed or interrupted delivery resolved itself (agent::Resolved): journal it, and tell the user when the turn will not
/// answer or waits to retry. `cue`: what the island shows for a retry. The orange one leaves once no retried wake is
/// pending (it got through, or a restart made it history); either leaves when Agent is off (`agent_on`).
pub(super) fn report_resolutions<H: agent::harness::Harness>(b: &mut agent::Bridge<H>, diagnostics: &heimdall_archive::records::Writer, ui: Option<ui::Ui>, cue: &mut RetryCue, agent_on: bool) {
    let now = agent::now_ms();
    for r in b.take_resolutions() {
        crate::diagnostics::record(diagnostics, "agent_delivery_resolved", "agent:pi", json!({"id":r.id,"outcome":format!("{:?}",r.outcome),"reason":r.reason}));
        let wait = b.retry_due(&r.id).ok().flatten().map(|at| at.saturating_sub(now));
        for c in resolution_cues(&r, wait) {
            *cue = match c { ui::HealthCue::Restarting { .. } => RetryCue::Retrying, ui::HealthCue::Problem { .. } => RetryCue::GaveUp, _ => RetryCue::None };
            if let Some(u) = ui { u.send(ui::UiUpdate::Health(c)); }
        }
    }
    if (*cue == RetryCue::Retrying && !b.retry_pending()) || (*cue != RetryCue::None && !agent_on) {
        *cue = RetryCue::None;
        if let Some(u) = ui { u.send(ui::UiUpdate::Health(ui::HealthCue::Clear { key: RETRY_KEY.into() })); }
    }
}


#[cfg(test)]
mod carry_tests {
    use super::*;
    /// Audit 27 Oct, F6 (review): Pi's raised in-turn threshold lasts only until the carried history is summarised; the
    /// first successful compaction asks for a reload once, an aborted or failed one does not.
    #[test]
    fn the_first_successful_compaction_ends_a_carry_once() {
        let luna = agent::provider::Limits { window: 272_000, ..agent::provider::Limits::for_context(128_000, Some(128_000)) };
        let carried = agent::maintenance::CompactionPolicy::for_limits(luna.carrying(Some(230_000)), false);
        assert!(carried.carrying && !agent::maintenance::CompactionPolicy::for_limits(luna, false).carrying);
        let done = json!({"type":"compaction_end","result":{"summary":"s"},"aborted":false});
        let after = carry_ended(Some(carried), &done).expect("a reload is due");
        assert!(!after.carrying);
        assert_eq!(carry_ended(Some(after), &done), None, "once");
        assert_eq!(carry_ended(Some(carried), &json!({"type":"compaction_end","aborted":true,"result":null})), None);
        assert_eq!(carry_ended(Some(carried), &json!({"type":"compaction_end","result":null,"errorMessage":"timed out"})), None);
        assert_eq!(carry_ended(Some(carried), &json!({"type":"compaction_start"})), None);
        assert_eq!(carry_ended(None, &done), None);
    }
}
