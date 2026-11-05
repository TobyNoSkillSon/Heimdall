//! The live app loop: capture run and its event pump, permissions, settings reload, listening and screen
//! start/stop and recovery.
use super::*;

pub(super) struct CaptureRun {
    pub(super) capture: Option<capture::Capture>,
    pub(super) pump: Option<std::thread::JoinHandle<()>>,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) draining: Arc<AtomicBool>,
}
/// Runs `start` on its own thread and hands its result back on the returned channel at once: the live loop never waits
/// for it (`CaptureRun::begin`).
pub(super) fn off_loop<T: Send + 'static>(name: &str, start: impl FnOnce() -> Result<T> + Send + 'static) -> crossbeam_channel::Receiver<Result<T>> {
    let (tx, rx) = bounded(1);
    let failed = tx.clone();
    if let Err(e) = std::thread::Builder::new().name(name.into()).spawn(move || { let _ = tx.send(start()); }) {
        let _ = failed.send(Err(anyhow::anyhow!("{name} did not start: {e}")));
    }
    rx
}
impl CaptureRun {
    /// Capture starting off the live loop (its HAL discovery waits up to 10 s): the result arrives on
    /// `app.capture_starting` and `adopt` makes the run. One start at a time.
    pub(super) fn begin(app: &mut pipeline::Pipeline, permissions: LivePermissions) -> Result<()> {
        if app.capture_starting.is_some() { return Ok(()); }
        let (config, consent, archive) = (mic_config(app, app.session.clone()), permissions.consent(), app.raw_archive()?);
        app.capture_starting = Some(off_loop("heimdall-capture-start", move || capture::Capture::start_with_archive(config, consent, archive)));
        Ok(())
    }
    /// The started capture as a run: its event pump and echo guard.
    pub(super) fn adopt(app: &pipeline::Pipeline, tx: crossbeam_channel::Sender<capture::CaptureEvent>, capture: capture::Capture, rx: capture::CaptureReceiver) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let draining = Arc::new(AtomicBool::new(false));
        let drain = draining.clone();
        let pump = std::thread::spawn(move || pump_capture_events(
            || rx.recv_timeout(Duration::from_secs(1)), tx, s, drain));
        if let Err(e) = crate::bridge::echo_cancel::spawn(capture.echo_control(), Arc::downgrade(&app.tools), stop.clone()) {
            eprintln!("Echo cancellation control did not start: {e}");
        }
        Self { capture: Some(capture), pump: Some(pump), stop, draining }
    }
    pub(super) fn start(
        app: &pipeline::Pipeline,
        tx: crossbeam_channel::Sender<capture::CaptureEvent>,
        permissions: LivePermissions,
    ) -> Result<Self> {
        let (capture, rx) = capture::Capture::start_with_archive(
            mic_config(app, app.session.clone()),
            permissions.consent(),
            app.raw_archive()?,
        )?;
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let draining = Arc::new(AtomicBool::new(false));
        let drain = draining.clone();
        let pump = std::thread::spawn(move || pump_capture_events(
            || rx.recv_timeout(Duration::from_secs(1)), tx, s, drain));
        // Echo guard (23 Oct): Apple's echo cancellation while Heimdall speaks on speakers (bridge/echo_cancel.rs).
        // It stops with the capture run (`stop`).
        if let Err(e) = crate::bridge::echo_cancel::spawn(capture.echo_control(), Arc::downgrade(&app.tools), stop.clone()) {
            eprintln!("Echo cancellation control did not start: {e}");
        }
        Ok(Self {
            capture: Some(capture),
            pump: Some(pump),
            stop,
            draining,
        })
    }
    /// Stop the native owner while continuing to receive its resampler tail and
    /// SourceRemoved events. The normal Drop cancellation intentionally abandons
    /// forwarding; Quit must drain before shutting Listen down.
    pub(super) fn finish(mut self, app: &mut pipeline::Pipeline, events: &crossbeam_channel::Receiver<capture::CaptureEvent>) {
        self.draining.store(true, Ordering::Release);
        let closing = std::thread::spawn(move || {
            self.capture.take();
            if let Some(pump) = self.pump.take() { let _ = pump.join(); }
        });
        while !closing.is_finished() {
            if let Ok(event) = events.recv_timeout(Duration::from_millis(25)) {
                if let Err(e) = app.capture_event(event) {
                    app.errors += 1;
                    app.record_teardown_error(&format!("Capture drain failed during shutdown: {e:#}"));
                    eprintln!("Capture drain failed; continuing teardown: {e:#}");
                }
            }
        }
        let _ = closing.join();
    }
}
/// Capture runs stopping off the live loop (`CaptureRun::retire`); Quit waits for them (`settle_retired`).
fn retiring() -> &'static std::sync::Mutex<Vec<std::thread::JoinHandle<()>>> {
    static RETIRING: std::sync::OnceLock<std::sync::Mutex<Vec<std::thread::JoinHandle<()>>>> = std::sync::OnceLock::new();
    RETIRING.get_or_init(Default::default)
}
impl CaptureRun {
    /// Stops this run without the live loop waiting for it (bug note 2026-10-27T16-57-38: a capture restart joined
    /// the capture thread on the live loop for 2.1 s while its last commit waited on the archive lock, and the hang
    /// watchdog fired). `forward`: the stopped run's last events (resampler tails, its sources' removals) still reach
    /// the live loop, which keeps running; the restart's case, where the new run's sources carry new ids
    /// (capture's process-wide generations). Without it they are abandoned, as a Listening off that clears every
    /// source wants.
    pub(super) fn retire(mut self, forward: bool) {
        let capture = self.capture.take();
        let pump = self.pump.take();
        // `Drop` below sets a stop nobody reads; the run's own stop is set here or after its capture has stopped.
        let stop = std::mem::replace(&mut self.stop, Arc::new(AtomicBool::new(false)));
        if !forward { stop.store(true, Ordering::Release); }
        let stop_after = stop.clone();
        let finish = move || {
            drop(capture);
            // Forwarding: the pump ends by itself once the stopped run's receiver disconnects, after its last events
            // (tails, each old source's removal) reached the live loop, even behind a full channel (review 27 Oct:
            // a stop set here dropped them, and an old source with its process-wide id was never removed). Bounded,
            // in case the live loop no longer reads.
            if let Some(pump) = &pump {
                let deadline = Instant::now() + RETIRE_FORWARD;
                while !pump.is_finished() && Instant::now() < deadline { std::thread::sleep(Duration::from_millis(10)); }
            }
            stop_after.store(true, Ordering::Release);
            if let Some(pump) = pump { let _ = pump.join(); }
        };
        // A thread that cannot start drops `finish` unrun: the capture then stops right here, as before.
        match std::thread::Builder::new().name("heimdall-capture-stop".into()).spawn(finish) {
            Ok(handle) => {
                let mut retiring = retiring().lock().unwrap();
                retiring.retain(|h| !h.is_finished());
                retiring.push(handle);
            }
            Err(e) => {
                eprintln!("Capture stop thread did not start ({e}); stopping on the live loop");
                stop.store(true, Ordering::Release);
            }
        }
    }
}
/// The longest a retired run's pump keeps forwarding its last events before it is stopped.
const RETIRE_FORWARD: Duration = Duration::from_secs(5);
/// Waits up to `timeout` for retired capture runs to stop (Quit: their last audio commits). True when none is left.
pub(super) fn settle_retired(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let left: Vec<_> = std::mem::take(&mut *retiring().lock().unwrap());
        let (done, running): (Vec<_>, Vec<_>) = left.into_iter().partition(|h| h.is_finished());
        for h in done { let _ = h.join(); }
        if running.is_empty() { return true; }
        retiring().lock().unwrap().extend(running);
        if Instant::now() >= deadline { return false; }
        std::thread::sleep(Duration::from_millis(10));
    }
}
pub(super) fn pump_capture_events(
    mut receive: impl FnMut() -> std::result::Result<capture::CaptureEvent, crossbeam_channel::RecvTimeoutError>,
    tx: crossbeam_channel::Sender<capture::CaptureEvent>,
    s: Arc<AtomicBool>,
    draining: Arc<AtomicBool>,
) {
    let mut gaps = std::collections::HashSet::new();
    while !s.load(Ordering::Acquire) {
        match receive() {
            Ok(mut e) => {
                if let capture::CaptureEvent::Audio(c) = &mut e {
                    c.discontinuity |= gaps.remove(&c.source_id);
                    let source = c.source_id.clone();
                    if draining.load(Ordering::Acquire) {
                        if tx.send(e).is_err() { break; }
                        continue;
                    }
                    match tx.try_send(e) {
                        Ok(()) => {}
                        Err(crossbeam_channel::TrySendError::Full(_)) => {
                            gaps.insert(source);
                        }
                        Err(_) => break,
                    }
                } else {
                    if let capture::CaptureEvent::SourceRemoved { source_id, .. } = &e {
                        gaps.remove(source_id);
                    }
                    let mut event = e;
                    loop {
                        match tx.send_timeout(event, Duration::from_millis(100)) {
                            Ok(()) => break,
                            Err(crossbeam_channel::SendTimeoutError::Timeout(e)) => {
                                if s.load(Ordering::Acquire) {
                                    return;
                                }
                                event = e;
                            }
                            Err(_) => return,
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(_) => break,
        }
    }
}
impl Drop for CaptureRun {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.capture.take();
        if let Some(t) = self.pump.take() {
            let _ = t.join();
        }
    }
}
/// Microphone state from capture or discovery, for the Microphone menu.
pub(super) fn microphone_reports() -> &'static (crossbeam_channel::Sender<capture::MicrophoneState>, crossbeam_channel::Receiver<capture::MicrophoneState>) {
    static REPORTS: std::sync::OnceLock<(crossbeam_channel::Sender<capture::MicrophoneState>, crossbeam_channel::Receiver<capture::MicrophoneState>)> = std::sync::OnceLock::new();
    REPORTS.get_or_init(unbounded)
}
/// App Audio: the apps playing now, from capture or discovery.
pub(super) type PlayingReports = (crossbeam_channel::Sender<Vec<capture::AppIdentity>>, crossbeam_channel::Receiver<Vec<capture::AppIdentity>>);
pub(super) fn playing_reports() -> &'static PlayingReports {
    static REPORTS: std::sync::OnceLock<PlayingReports> = std::sync::OnceLock::new();
    REPORTS.get_or_init(unbounded)
}
pub(super) fn mic_config(app: &pipeline::Pipeline, archive_dir: PathBuf) -> capture::CaptureConfig {
    let (microphone, extra_microphones) = app.settings.microphone_uids();
    capture::CaptureConfig {
        archive_dir,
        microphone,
        extra_microphones,
        microphones: Some(microphone_reports().0.clone()),
        recorded_apps: app.settings.recorded_app_keys(),
        playing_apps: Some(playing_reports().0.clone()),
        // The public edition starts on macOS's default input (5 Nov).
        system_default_input: crate::edition::PUBLIC,
    }
}
pub(super) fn apply_recorded_apps(app: &pipeline::Pipeline, capture: &Option<CaptureRun>, discovering: &Option<capture::Capture>) {
    let keys = app.settings.recorded_app_keys();
    for c in capture.as_ref().and_then(|c| c.capture.as_ref()).into_iter().chain(discovering.as_ref()) {
        c.set_recorded_apps(keys.clone());
    }
}
/// Whether an app checked in App Audio is playing now: only then does a silent tap say anything about System Audio.
pub(super) fn recording_playing(app: &pipeline::Pipeline, playing: &[capture::AppIdentity]) -> bool {
    let keys = app.settings.recorded_app_keys();
    playing.iter().any(|a| keys.contains(&a.key))
}
pub(super) fn apply_microphones(app: &pipeline::Pipeline, capture: &Option<CaptureRun>, discovering: &Option<capture::Capture>) {
    let (primary, extras) = app.settings.microphone_uids();
    for c in capture.as_ref().and_then(|c| c.capture.as_ref()).into_iter().chain(discovering.as_ref()) {
        c.set_microphones(primary.clone(), extras.clone());
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LivePermissions {
    pub(super) mic: bool, pub(super) system: bool, pub(super) screen: bool, pub(super) accessibility: bool, pub(super) system_confirmed: bool,
    /// Host time of the first digital-silence tap chunk while a recorded app plays; `None` once it stops or sound arrives.
    pub(super) silent_since: Option<u64>,
    pub(super) system_denied: bool,
}
/// A denied System Audio Recording tap delivers digital silence. A recorded app playing for this long while every tap
/// stays exactly silent reads as denied; the first real sound turns it Allowed.
pub(super) const SYSTEM_AUDIO_DENIAL_NS: u64 = 10_000_000_000;
impl LivePermissions {
    pub(super) fn observe(&mut self, states: &[(ui::Permission, ui::PermissionState)]) {
        for (p,s) in states {
            let granted = *s == ui::PermissionState::Granted;
            match p {
                ui::Permission::Microphone => self.mic = granted,
                // No preflight API. Explicit setup consent permits a tap attempt.
                // Denied is inferred from silent taps, not reported by macOS: the taps keep running so the grant can be seen.
                ui::Permission::SystemAudio => {
                    self.system = granted || matches!(s, ui::PermissionState::Unconfirmed | ui::PermissionState::Denied);
                    self.system_confirmed = granted;
                },
                ui::Permission::ScreenRecording => self.screen = granted,
                ui::Permission::Accessibility => self.accessibility = granted,
            }
        }
    }
    pub(super) fn consent(self) -> capture::CaptureConsent { capture::CaptureConsent::after_setup(self.mic,self.system) }
    pub(super) fn desired(self, enabled: bool) -> [bool;4] { [enabled && self.mic, enabled && self.system, enabled && self.screen, enabled && self.accessibility] }
    /// System Audio has no status call: real sound from a tap grants it; digital silence while a recorded app plays
    /// (`recorded_playing`) for `SYSTEM_AUDIO_DENIAL_NS` denies it.
    pub(super) fn audio_permission_transition(&mut self, source_id: &str, samples: &[f32], host_ns: u64, recorded_playing: bool) -> Option<ui::UiUpdate> {
        if source_id.starts_with("mic:") || self.system_confirmed { return None; }
        let state = if samples.iter().any(|s| *s != 0.0) {
            self.system_confirmed = true;
            ui::PermissionState::Granted
        } else if !recorded_playing {
            self.silent_since = None;
            return None;
        } else {
            let since = *self.silent_since.get_or_insert(host_ns);
            if self.system_denied || host_ns.saturating_sub(since) < SYSTEM_AUDIO_DENIAL_NS { return None; }
            self.system_denied = true;
            ui::PermissionState::Denied
        };
        Some(ui::UiUpdate::Permissions(vec![(ui::Permission::SystemAudio, state)]))
    }
}
pub(super) fn live(
    data: PathBuf,
    models: PathBuf,
    s: Settings,
    u: ui::Ui,
    actions: crossbeam_channel::Receiver<ui::UiAction>,
    tools: Arc<bridge::Tools>,
    beats: Arc<watchdog::Beats>,
) -> Result<()> {
    beats.arm_live();
    watchdog::watch_quit_progress(&beats);
    let relaunched = watchdog::RecoveryLog::take_pending(&data);
    let mut app = pipeline::Pipeline::new_with_tools(data, models, s, Some(u), false, tools)?;
    app.relaunched = relaunched;
    if app.settings.setup_requested { ui::permissions::restore_setup_requested(); }
    app.initial_ui();
    app.start_inference_engine();
    if app.settings.setup_requested {
        // Audio already seen once stays Granted: with App Audio opt-in, nothing may play for hours.
        let state = if app.settings.system_audio_confirmed { ui::PermissionState::Granted } else { ui::PermissionState::Unconfirmed };
        u.send(ui::UiUpdate::Permissions(vec![(ui::Permission::SystemAudio,state)]));
    }
    let playing = playing_reports().1.clone();
    let (mut playing_now, mut recorded_playing) = (Vec::new(), false);
    let mut heard: Vec<settings::HeardApp> = vec![];
    u.send(ui::UiUpdate::AppAudio(app.menu_apps(&heard)));
    let (tx, events) = bounded(512);
    let (listen_tx, listen_events) = bounded(512);
    let mut worker_started = false;
    let mut capture = None;
    let mut screens = None;
    let mut discovering = Some(capture::Capture::discover(&mic_config(&app, app.session.clone()))?);
    let microphones = microphone_reports().1.clone();
    let mut seen_microphones = capture::MicrophoneState::default();
    // Listening off survives relaunch: no capture starts, and the archive records why there is no audio. The agent
    // and the screen still start below, on their own switches.
    // The public edition's downloads: a launch after Setup resumes any not finished; listening waits for its models
    // (pipeline/model_downloads.rs) and the settings tick starts it when they land.
    if crate::setup_wizard::downloads_wanted(&app.settings) { app.start_model_downloads(); }
    app.models_tick();
    let mut wants_listen = app.settings.setup_requested && !app.settings.paused && app.listen_models_ready();
    if app.settings.setup_requested && app.settings.paused {
        app.set_running(false, "launched off")?;
        u.send(ui::UiUpdate::Listen(ui::ListenState::Paused));
    }
    let mut quitting = false;
    // Start only after persisted setup consent AND passive grants; no startup request.
    let mut permissions = LivePermissions {
        system: app.settings.setup_requested,
        system_confirmed: app.settings.setup_requested && app.settings.system_audio_confirmed,
        ..Default::default()
    };
    permissions.observe(&ui::permissions::status());
    if wants_listen {
        app.listening_starting();
        start_live(
            &mut app,
            &mut capture,
            &mut screens,
            &tx,
            &listen_tx,
            &mut worker_started,
            permissions,
        )?;
        discovering.take();
    } else if app.settings.setup_requested {
        // Launched with Listening off: the agent and the screen run on their own switches.
        start_unheard(&mut app, &mut screens, permissions);
    }
    let maintenance = crossbeam_channel::tick(Duration::from_secs(3600));
    let settings_tick = crossbeam_channel::tick(Duration::from_secs(2));
    let mut settings_watch = settings::SettingsWatch::default();
    let internal = app.internal_events();
    // Engine model downloads (pipeline/engine_models.rs): progress reaches the menu as it comes, not on the 2 s tick.
    let engine_downloads = app.engine_download_receiver();
    let mut last_tick = Instant::now();
    let mut screen_ladder = health::Ladder::new(Duration::from_secs(600));
    // Started: the install tool keeps an update from here (public edition from its bundle; a no-op otherwise).
    crate::updates::report(heimdall_update::readiness::Phase::Ready, "");
    // Vella's dictation state (vella.rs, docs/vella.md): its notifications arrive here stamped at receipt, and Vella is
    // asked for a hello; the settings tick ends a dictation not heard from in 45 s. `vella_tx` lives as long as the loop.
    let (vella_tx, vella_rx) = bounded::<crate::vella::Heard>(64);
    #[cfg(target_os = "macos")]
    {
        let tx = vella_tx.clone();
        u.on_main(move || crate::vella::observe(tx));
    }
    // Vella borrows the loaded listen model over asr.sock (vella_asr.rs); a failure here only costs Vella the loan.
    let mut vella_asr = Some(crate::vella_asr::VellaAsr::start(&crate::vella_asr::socket_dir(&app.data, crate::settings::profile_data().as_deref(), std::env::var_os("HOME").map(std::path::PathBuf::from).as_deref(), crate::edition::PUBLIC), true)).map(|(v, e)| { if let Some(e) = e { eprintln!("Vella transcription socket (retrying every 30 s): {e:#}"); } v });
    while !quitting {
        if let Some(v) = &mut vella_asr { v.sync(app.live_worker()); }
        app.night_admission(app.settings.setup_requested && (!wants_listen || worker_started));
        let speaker_ready = app.speaker_events_rx.clone();
        let screen_events = screens.as_ref().map(|s: &screen::ScreenCapture| s.events.clone()).unwrap_or_else(crossbeam_channel::never);
        // A drop onto the notch nobody's words took goes alone when due (pipeline/drops.rs).
        let drop_due = app.drop_deadline().map(crossbeam_channel::at).unwrap_or_else(crossbeam_channel::never);
        let capture_ready = app.capture_starting.clone().unwrap_or_else(crossbeam_channel::never);
        select! {
            recv(capture_ready)->started=>{
                let started=started.unwrap_or_else(|_| Err(anyhow::anyhow!("the capture start thread ended")));
                let wanted=wants_listen&&app.settings.setup_requested;
                adopt_capture(&mut app,&mut capture,&tx,started,wanted);
            },
            recv(internal)->e=>if let Err(e)=app.internal(e?){app.note_line(&format!("Agent pre-flight: {e:#}"));},
            recv(drop_due)->_=>if let Err(e)=app.drop_tick(){app.note_line(&format!("Drop: {e:#}"));},
            recv(engine_downloads)->e=>if let Ok(e)=e {app.engine_download_event(e);},
            recv(settings_tick)->_=>{
                // The watchdog's heartbeat: this tick is handled whatever else happens, idle or busy.
                beats.live_beat();
                if let Err(e) = adopt_screen(&mut app, &mut screens, Duration::ZERO) { app.fail(&format!("Screen: {e:#}")); }
                app.health_tick(last_tick.elapsed(), &listen_tx);
                last_tick = Instant::now();
                // Queued heartbeats first: `select!` picks among ready channels at random (vella.rs).
                drain_vella(&mut app, &vella_rx);
                app.vella_tick();
                app.catch_up_tick();
                // Health and Jarvis's residency also without capture events (Listening off).
                app.component_tick();
                // `Heimdall setup`'s requests (setup_cli.rs), then Getting ready's rows and the status file.
                app.setup_requests_tick();
                app.setup_ready_tick();
                // Listening's models landed (or went): listening follows, no relaunch.
                if app.models_tick() {
                    let now = app.settings.setup_requested && !app.settings.paused && app.listen_models_ready();
                    if now != wants_listen {
                        wants_listen = now;
                        reconcile_listening(&mut app,wants_listen,&mut capture,&mut screens,&mut discovering,&events,&listen_events,&tx,&listen_tx,&mut worker_started,permissions,u)?;
                    }
                }
                if let Some(old) = reload_settings(&mut app, &mut settings_watch) {
                apply_microphones(&app,&capture,&discovering);
                apply_recorded_apps(&app,&capture,&discovering);
                u.send(ui::UiUpdate::Microphones(app.menu_microphones(&seen_microphones)));
                u.send(ui::UiUpdate::AppAudio(app.menu_apps(&heard)));
                if old.screen_off != app.settings.screen_off || old.setup_requested != app.settings.setup_requested {
                    let setup=app.settings.setup_requested;reconcile_screen(&mut app,&mut screens,setup,permissions);
                }
                if old.screen != app.settings.screen {
                    if let Some(s) = &screens {
                        if let Err(e) = s.reconfigure(app.settings.screen.config(&ui::display_sizes())) { app.settings_result("Screen Recording",Some(format!("{e:#}"))); }
                    }
                }
                if old.paused != app.settings.paused || old.setup_requested != app.settings.setup_requested {
                    wants_listen = app.settings.setup_requested && !app.settings.paused && app.listen_models_ready();
                    reconcile_listening(&mut app,wants_listen,&mut capture,&mut screens,&mut discovering,&events,&listen_events,&tx,&listen_tx,&mut worker_started,permissions,u)?;
                    if !wants_listen && app.settings.setup_requested { if let Err(e)=app.ensure_agent(){app.fail(&format!("Agent: {e:#}"));} }
                }
            }},
            recv(vella_rx)->heard=>if let Ok(heard)=heard{app.vella_notice(heard);},
            recv(speaker_ready)->e=>if let Err(e)=app.speaker_update(e?){app.errors+=1;app.fail(&format!("Speaker activation failed: {e:#}"));},
            recv(screen_events)->event=>{
                match event {
                    Ok(event) => app.screen_event(event),
                    Err(_) => {
                        screens.take(); *app.tools.buffer.lock().unwrap() = None;
                        app.screen_event(screen::ScreenEvent::Error("screen stream owner ended".into()));
                        if screen_ladder.climb(Instant::now()) == health::Rung::Part {
                            app.request_recovery(health::Recovery::RestartScreen("screen stream owner ended".into()));
                        } else {
                            app.screen_event(screen::ScreenEvent::Unavailable);
                        }
                    },
                }
            },
            recv(microphones)->m=>{
                seen_microphones=m?;
                app.microphone_state(&seen_microphones);
                u.send(ui::UiUpdate::Microphones(app.menu_microphones(&seen_microphones)));
            },
            recv(playing)->apps=>{
                playing_now=apps?;
                recorded_playing=recording_playing(&app,&playing_now);
                settings::merge_heard(&mut heard,&playing_now,&app.settings.recorded_app_keys());
                u.send(ui::UiUpdate::AppAudio(app.menu_apps(&heard)));
            },
            recv(maintenance)->_=>{
                archive::prune(&app.data.join("archive"),app.settings.retention_days,Some(&app.session))?;
            },
            recv(actions)->a=>match a? {
                ui::UiAction::Quit=>{app.request_engine_stop();if let Some(d)=crate::model_store::global(){d.stop();}quitting=true;},
                ui::UiAction::Reload=>{
                    if let Err(e)=app.retry_settings(){app.note_line(&format!("Reload: {e:#}"));}
                    if app.pending_reload.contains_key("Screen Recording") {
                        if let Some(s)=&screens { if let Err(e)=s.reconfigure(app.settings.screen.config(&ui::display_sizes())) {app.settings_result("Screen Recording",Some(format!("{e:#}")));} }
                    }
                },
                ui::UiAction::SelectMicrophone{id}=>{
                    app.settings.choose_microphone(&id,&seen_microphones);app.save_or_note();
                    apply_microphones(&app,&capture,&discovering);
                    u.send(ui::UiUpdate::Microphones(app.menu_microphones(&seen_microphones)));
                },
                ui::UiAction::SetMicrophoneOn{id,on}=>{
                    app.settings.set_microphone_on(&id,on,&seen_microphones);app.save_or_note();
                    apply_microphones(&app,&capture,&discovering);
                    u.send(ui::UiUpdate::Microphones(app.menu_microphones(&seen_microphones)));
                },
                ui::UiAction::SetAppRecorded{id,on}=>{
                    app.settings.set_app_recorded(&id,on,&mut heard);app.save_or_note();
                    recorded_playing=recording_playing(&app,&playing_now);
                    apply_recorded_apps(&app,&capture,&discovering);
                    u.send(ui::UiUpdate::AppAudio(app.menu_apps(&heard)));
                },
                ui::UiAction::SetupAllowed=>{if let Err(e)=app.heimdall_intent_processed(){app.note_save_failure(&e);}},
                ui::UiAction::RequestSystemAudio=>{
                    // This callback exists only after the setup Allow All button; its On intent was SetupAllowed.
                    // A failed save is noted and retried by the settings tick; it never ends the live loop.
                    if let Err(e)=app.setup_requested(){app.note_save_failure(&e);}
                    // Allow All also (re)starts the model downloads, so listening's models are on their way.
                    app.start_model_downloads();
                    wants_listen = !app.settings.paused && app.listen_models_ready();
                    permissions.system=true;permissions.observe(&ui::permissions::status());
                    if wants_listen {start_live(&mut app,&mut capture,&mut screens,&tx,&listen_tx,&mut worker_started,permissions)?;discovering.take();}
                    else {start_unheard(&mut app,&mut screens,permissions);}
                    u.send(ui::UiUpdate::Permissions(vec![(ui::Permission::SystemAudio,ui::PermissionState::Unconfirmed)]));
                },
                ui::UiAction::PermissionsChanged(states)=>{
                    let previous=permissions;permissions.observe(&states);
                    if app.settings.setup_requested { permission_health(&mut app, previous, permissions); }
                    if wants_listen && app.settings.setup_requested {
                        start_live(&mut app,&mut capture,&mut screens,&tx,&listen_tx,&mut worker_started,permissions)?;
                        if !previous.accessibility && permissions.accessibility { app.refresh_discord_accessibility()?; }
                    } else if app.settings.setup_requested {
                        // Listening off: Screen Recording granted or revoked still starts or stops Seeing.
                        reconcile_screen(&mut app,&mut screens,true,permissions);
                    }
                },
                ui::UiAction::RetrySystemAudio=>{
                    if wants_listen && app.settings.setup_requested {
                        if let Some(c) = capture.as_ref().and_then(|c: &CaptureRun| c.capture.as_ref()) {
                            c.refresh_permissions(permissions.consent());
                        } else if permissions.system {
                            if let Err(e)=CaptureRun::begin(&mut app,permissions){app.fail(&format!("Capture: {e:#}"));}
                        }
                    }
                },
                ui::UiAction::SetListening(_)=>{
                    // The latch already holds the intent; a failed save is retried by the settings tick.
                    if let Err(e)=app.heimdall_intent_processed(){app.note_save_failure(&e);}
                    wants_listen=!app.settings.paused&&app.settings.setup_requested&&app.listen_models_ready();
                    reconcile_listening(&mut app,wants_listen,&mut capture,&mut screens,&mut discovering,&events,&listen_events,&tx,&listen_tx,&mut worker_started,permissions,u)?;
                    if !app.settings.paused&&!app.settings.setup_requested{u.send(ui::UiUpdate::ShowSetup);}
                },
                ui::UiAction::SetSeeing(on)=>{
                    if let Err(e)=app.set_seeing(on){screen_setting_rejected(&mut app,"Seeing",&e);continue;}
                    let setup=app.settings.setup_requested;reconcile_screen(&mut app,&mut screens,setup,permissions);
                },
                ui::UiAction::SetScreenRecording(choice)=>{
                    let mut next=app.settings.clone();next.screen=settings::ScreenSettings::from_ui(choice);
                    if let Err(e)=app.apply_menu_settings(next){screen_setting_rejected(&mut app,"Screen",&e);continue;}
                    app.save_or_note();
                    app.runtime_settings_ui();
                    if let Some(s)=&screens {if let Err(e)=s.reconfigure(app.settings.screen.config(&ui::display_sizes())){app.settings_result("Screen Recording",Some(format!("{e:#}")));}}
                },
                a=>if let Err(e)=app.action(a){app.note_line(&format!("Action failed: {e}"));},
            },
            recv(listen_events)->e=>{drain_vella(&mut app, &vella_rx);if let Some(state)=microphones.try_iter().last(){seen_microphones=state;app.microphone_state(&seen_microphones);u.send(ui::UiUpdate::Microphones(app.menu_microphones(&seen_microphones)));}match e? {
                Some(event)=>if let Err(e)=app.worker_event(event){app.errors+=1;app.fail(&format!("Pipeline error: {e:#}"));},
                // The worker ended on its own: recorded, shown on the island, restarted while listening.
                None=>if let Err(e)=app.worker_ended(wants_listen.then_some(&listen_tx)){app.errors+=1;app.fail(&format!("Listen: {e:#}"));},
            }},
            recv(events)->e=>{
                if let Some(state)=microphones.try_iter().last(){seen_microphones=state;app.microphone_state(&seen_microphones);u.send(ui::UiUpdate::Microphones(app.menu_microphones(&seen_microphones)));}
                let e=e?;
                if let capture::CaptureEvent::Audio(c)=&e {
                    if let Some(update)=permissions.audio_permission_transition(&c.source_id.0,&c.samples,c.host_ns,recorded_playing){
                        system_audio_health(&mut app, &update);
                        u.send(update);
                        if permissions.system_confirmed&&!app.settings.system_audio_confirmed{app.settings.system_audio_confirmed=true;app.save_or_note();}
                    }
                }
                if wants_listen {if matches!(e,capture::CaptureEvent::Audio(_))&&!worker_started{app.ensure_live_worker(listen_tx.clone())?;worker_started=true;}
                    if let Err(e)=app.capture_event(e){app.errors+=1;app.fail(&format!("Pipeline error: {e:#}"));}}
            }
        }
        if let Some(r) = app.take_recovery() {
            recover(r, &mut app, &mut capture, &mut screens, &mut discovering, &events, &listen_events, &tx, &listen_tx, &mut worker_started, permissions, u, wants_listen, &mut quitting)?;
        }
    }
    // Shutting down on purpose: the watchdog bounds it (Quit always ends) instead of watching it as a freeze.
    beats.quitting();
    drop(vella_asr.take()); // `gone` to Vella, then the socket closes
    // Also for a recovery's Quit: no teardown write waits behind a compaction step (main.rs does it at the click).
    heimdall_archive::compact::preempt_all();
    // Floating islands (night 1022): the bundled helper quits with Heimdall.
    crate::islands::live::shutdown();
    app.night_admission(false);
    if let Some(capture) = capture.take() { capture.finish(&mut app, &events); }
    beats.quit_progress();
    if !settle_retired(Duration::from_secs(4)) {
        eprintln!("Quit: a restarted capture is still stopping; its last second may be lost");
    }
    beats.quit_progress();
    // Listening off hands a slow last commit to the background; Quit lets it land.
    if !capture::settle_stopped_archives(Duration::from_secs(4)) {
        eprintln!("Quit: a stopped capture's archive is still committing; its last second may be lost");
    }
    while let Ok(e) = events.try_recv() {
        if let Err(e)=app.capture_event(e){app.errors+=1;app.record_teardown_error(&format!("Capture drain failed during shutdown: {e:#}"));eprintln!("Capture drain failed; continuing teardown: {e:#}");}
    }
    beats.quit_progress();
    app.stop_live_worker(&listen_events)?;
    beats.quit_progress();
    // The last stream stops in the lane, after any earlier stop; Quit gives the lane a bounded moment so its final
    // gap lands before the session is marked complete.
    cancel_screen_start(&mut app);
    if let Some(s) = screens.take() { screen_lane(&mut app, move || drop(s)); }
    let lane_deadline = Instant::now() + Duration::from_secs(4);
    while app.screen_lane.as_ref().is_some_and(|l| !l.is_finished()) && Instant::now() < lane_deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(discovering);
    beats.quit_progress();
    app.finish()?;
    u.send(ui::UiUpdate::Terminate);
    Ok(())
}
/// Carries out a recovery the pipeline asked for (pipeline/resilience.rs): the lower rungs in-process, the top one
/// by relaunching through a normal Quit, within the relaunch bound.
#[allow(clippy::too_many_arguments)]
pub(super) fn recover(
    r: health::Recovery, app: &mut pipeline::Pipeline, capture: &mut Option<CaptureRun>,
    screens: &mut Option<screen::ScreenCapture>, discovering: &mut Option<capture::Capture>,
    events: &crossbeam_channel::Receiver<capture::CaptureEvent>,
    listen_events: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>,
    tx: &crossbeam_channel::Sender<capture::CaptureEvent>,
    listen_tx: &crossbeam_channel::Sender<Option<listen::WorkerEvent>>,
    worker_started: &mut bool, permissions: LivePermissions, u: ui::Ui, wants_listen: bool, quitting: &mut bool,
) -> Result<()> {
    match r {
        health::Recovery::RestartCapture(why) => {
            if !wants_listen { return Ok(()); }
            app.note_line(&format!("Restarting capture: {why}"));
            if let Some(c) = capture.take() { c.retire(true); }
            app.capture_restarting();
            while let Ok(e) = events.try_recv() { if let Err(e) = app.capture_event(e) { eprintln!("Capture drain failed; restarting anyway: {e:#}"); } }
            if let Err(e) = CaptureRun::begin(app, permissions) { app.fail(&format!("Capture: {e:#}")); }
        }
        health::Recovery::RestartScreen(why) => {
            if !app.settings.setup_requested { return Ok(()); }
            app.note_line(&format!("Restarting screen capture: {why}"));
            // A stream whose owner gave up after its restarts takes the OS refresh path (its attempts start over);
            // with no stream, one starts.
            match screens.as_ref() {
                Some(s) => s.refresh(),
                None => reconcile_screen(app, screens, true, permissions),
            }
        }
        health::Recovery::RestartPipeline(why) => {
            if !wants_listen { return Ok(()); }
            app.note_line(&format!("Restarting listening: {why}"));
            reconcile_listening(app, false, capture, screens, discovering, events, listen_events, tx, listen_tx, worker_started, permissions, u)?;
            reconcile_listening(app, true, capture, screens, discovering, events, listen_events, tx, listen_tx, worker_started, permissions, u)?;
            // Off cleared the live conditions; this one stays until audio flows again.
            let cue = app.health.restarting("capture", ui::Part::Microphone, "Restarting listening…");
            app.health_cue(cue);
        }
        health::Recovery::Relaunch { key, part, problem } => {
            let data = app.data.clone();
            let allowed = watchdog::RecoveryLog::load(&data).is_ok_and(|log| log.allows(watchdog::unix_now()));
            let launched = allowed
                && watchdog::start_relaunch(&data, &problem, true, &app.tools)
                    .map_err(|e| {
                        app.tools.record_diagnostic("recovery", serde_json::json!({"state":"blocked", "reason":format!("{e:#}"), "trigger":problem}));
                        eprintln!("Relaunch failed: {e:#}");
                    })
                    .is_ok();
            if launched {
                app.note_line(&format!("{problem}; restarting Heimdall"));
                app.health_cue(Some(ui::HealthCue::Restarting { key: "heimdall".into(), part: ui::Part::Heimdall, label: "Restarting Heimdall…".into() }));
                *quitting = true;
            } else {
                let cue = app.health.problem(&key, part, &problem, "Click to restart Heimdall", ui::HealthControl::Relaunch);
                app.health_cue(cue);
            }
        }
    }
    Ok(())
}
/// A permission Heimdall uses was turned off in System Settings (or back on): red with a click to the pane, then
/// OK. Only for parts in use: Screen Recording while Seeing is on.
pub(super) fn permission_health(app: &mut pipeline::Pipeline, before: LivePermissions, now: LivePermissions) {
    use ui::Permission as P;
    let screen_on = !app.settings.screen_off;
    for (key, part, permission, label, had, has, relevant) in [
        ("permission:mic", ui::Part::Microphone, P::Microphone, "Microphone", before.mic, now.mic, true),
        ("permission:screen", ui::Part::Screen, P::ScreenRecording, "Screen Recording", before.screen, now.screen, screen_on),
        ("permission:accessibility", ui::Part::Heimdall, P::Accessibility, "Accessibility", before.accessibility, now.accessibility, true),
    ] {
        let cue = if had && !has && relevant {
            app.health.problem(key, part, &format!("{label} permission is off"), "Click to open Settings", ui::HealthControl::OpenSettings(permission))
        } else if has && !had {
            app.health.recovered(key, part, &format!("{label} permission back"))
        } else {
            None
        };
        app.health_cue(cue);
    }
}
/// System Audio has no status call: capture infers it from the taps (silence while a recorded app plays).
pub(super) fn system_audio_health(app: &mut pipeline::Pipeline, update: &ui::UiUpdate) {
    let ui::UiUpdate::Permissions(states) = update else { return };
    for (permission, state) in states {
        if *permission != ui::Permission::SystemAudio { continue; }
        let cue = match state {
            ui::PermissionState::Denied => app.health.problem("permission:system", ui::Part::Speech, "System Audio permission is off", "Click to open Settings", ui::HealthControl::OpenSettings(ui::Permission::SystemAudio)),
            ui::PermissionState::Granted => app.health.recovered("permission:system", ui::Part::Speech, "System Audio permission back"),
            _ => None,
        };
        app.health_cue(cue);
    }
}
/// The live settings watcher. Returns the previous settings when a file revision applied.
pub(super) fn reload_settings(app: &mut pipeline::Pipeline, watch: &mut settings::SettingsWatch) -> Option<Settings> {
    // An invalid settings.json keeps the last good settings, with a pill until a valid revision arrives.
    let invalid = |app: &mut pipeline::Pipeline, reason: &str| {
        app.component_status("settings", Some(reason.into()));
        let cue = app.health.dropped("settings", ui::Part::Settings, "settings.json is invalid · using the last good one");
        app.health_cue(cue);
    };
    let applied = match watch.read_changed(&app.data.join("settings.json")) {
        Ok(Some(mut next)) => {
            next.normalize_archive();
            let applied = if next == app.settings && !app.stop_settings_fresh(&next) { Ok(None) } else {
                let old = app.settings.clone();
                app.apply_settings(next).map(|()| Some(old))
            };
            match applied {
                Ok(old) => {
                    app.component_status("settings", None);
                    let cue = app.health.recovered("settings", ui::Part::Settings, "Settings OK");
                    app.health_cue(cue);
                    old
                }
                Err(e) => { app.note_line(&format!("Settings rejected: {e:#}")); invalid(app, &format!("{e:#}")); None }
            }
        }
        Ok(None) => None,
        Err(e) => { app.note_line(&format!("Settings rejected; keeping current settings: {e:#}")); invalid(app, &format!("{e:#}")); None }
    };
    // A failed save (or a stop value the file lacks) is rewritten on every tick until it lands.
    match app.persist_settings() {
        Ok(()) => app.component_status("settings-save", None),
        Err(e) => app.component_status("settings-save", Some(format!("Settings save failed; retrying: {e:#}"))),
    }
    applied
}
#[allow(clippy::too_many_arguments)]
pub(super) fn start_live(
    app: &mut pipeline::Pipeline,
    capture: &mut Option<CaptureRun>,
    screens: &mut Option<screen::ScreenCapture>,
    // Capture now starts off the loop (`CaptureRun::begin`); its sender is `adopt_capture`'s.
    _tx: &crossbeam_channel::Sender<capture::CaptureEvent>,
    listen_tx: &crossbeam_channel::Sender<Option<listen::WorkerEvent>>,
    worker_started: &mut bool,
    permissions: LivePermissions,
) -> Result<()> {
    app.night_admission(false);
    let desired = permissions.desired(true);
    let audio = desired[0] || desired[1];
    // Nothing here waits for a model: the listen worker loads on its own thread (or stayed loaded since the last
    // Off) while its audio queues, so the microphone and the screen start at once. Decisions load last.
    if audio {
        app.health_listening();
        if let Err(e) = app.ensure_live_worker(listen_tx.clone()) { app.fail(&format!("Listen: {e:#}")); }
        *worker_started = true;
        if capture.is_none() {
            // Off the live loop: `adopt_capture` takes it when it is up.
            if let Err(e) = CaptureRun::begin(app, permissions) { app.fail(&format!("Capture: {e:#}")); }
        } else if let Some(c) = capture.as_ref().and_then(|c| c.capture.as_ref()) { c.refresh_permissions(permissions.consent()); }
    } else {
        if let Some(c) = capture.take() { c.retire(false); }
        drop_starting_capture(app);
    }
    if desired[2] && !app.settings.screen_off {
        if let Err(e) = start_screen(app, screens) { app.fail(&format!("Screen: {e:#}")); }
    } else { stop_screen(app, screens, if app.settings.screen_off { "Seeing off" } else { "Screen Recording permission unavailable" }); }
    if audio {
        if let Err(e) = app.prepare_decisions() { app.fail(&format!("Decisions: {e:#}")); }
    } else { app.listening_not_starting(); }
    if let Some(u) = app.ui {
        u.send(ui::UiUpdate::Permissions(ui::permissions::status()));
        app.listening_status();
    }
    app.listening_ready_check();
    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(super) fn reconcile_listening(
    app: &mut pipeline::Pipeline, on: bool, capture: &mut Option<CaptureRun>,
    screens: &mut Option<screen::ScreenCapture>, discovering: &mut Option<capture::Capture>,
    events: &crossbeam_channel::Receiver<capture::CaptureEvent>,
    listen_events: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>,
    tx: &crossbeam_channel::Sender<capture::CaptureEvent>,
    listen_tx: &crossbeam_channel::Sender<Option<listen::WorkerEvent>>,
    worker_started: &mut bool, permissions: LivePermissions, u: ui::Ui,
) -> Result<()> {
    if on {
        app.set_running(true,"setting")?;
        app.listening_starting();
        start_live(app,capture,screens,tx,listen_tx,worker_started,permissions)?;
        discovering.take();
    } else {
        if let Some(c)=capture.take() { c.retire(false); }
        drop_starting_capture(app);
        while let Ok(e)=events.try_recv(){if let Err(e)=app.capture_event(e){app.errors+=1;eprintln!("Capture drain failed; continuing pause: {e:#}");}}
        app.stop_listening(listen_events)?;
        u.send(ui::UiUpdate::Listen(ui::ListenState::Paused));
        if discovering.is_none(){*discovering=capture::Capture::discover(&mic_config(app,app.session.clone())).ok();}
    }
    Ok(())
}
/// A refused Seeing or Screen Recording click: noted, and the menu redrawn from the settings in force, so its tile
/// leaves the state it showed on the click.
pub(super) fn screen_setting_rejected(app: &mut pipeline::Pipeline, what: &str, e: &anyhow::Error) {
    app.note_line(&format!("{what} setting rejected: {e:#}"));
    app.runtime_settings_ui();
}
/// Seeing on or off: on starts the buffer (once setup was allowed and Screen Recording is granted), off stops the
/// stream and frees every buffered frame. Listening has no say: each switch turns off only itself.
pub(super) fn reconcile_screen(app: &mut pipeline::Pipeline, screens: &mut Option<screen::ScreenCapture>, setup: bool, permissions: LivePermissions) {
    if !app.settings.screen_off && setup && permissions.desired(true)[2] {
        if let Err(e) = start_screen(app, screens) { app.fail(&format!("Screen: {e:#}")); }
    } else {
        stop_screen(app, screens, if app.settings.screen_off { "Seeing off" } else if !setup { "Setup not allowed" } else { "Screen Recording permission unavailable" });
    }
}
/// Setup allowed with Listening off: no capture, but the agent runs and Seeing records on its own switch.
pub(super) fn start_unheard(app: &mut pipeline::Pipeline, screens: &mut Option<screen::ScreenCapture>, permissions: LivePermissions) {
    if let Err(e) = app.ensure_agent() { app.fail(&format!("Agent: {e:#}")); }
    reconcile_screen(app, screens, true, permissions);
}
/// No stream, no frames: the pipeline's buffer is replaced by an empty one, so nothing stale is kept or attached.
pub(super) fn stop_screen(app: &mut pipeline::Pipeline, screens: &mut Option<screen::ScreenCapture>, reason: &str) {
    cancel_screen_start(app);
    if let Some(s) = screens.take() { screen_lane(app, move || drop(s)); }
    *app.tools.buffer.lock().unwrap() = None;
    app.clear_screen_frames();
    app.screen_stopped(reason);
}
/// A start still running is abandoned: whatever it starts is stopped in the lane, never on the live loop (dropping the
/// receiver here would drop a stream already sent into it).
pub(super) fn cancel_screen_start(app: &mut pipeline::Pipeline) {
    if let Some((rx, _, cancelled)) = app.screen_starting.take() {
        // A start still queued behind an earlier stop never starts at all: no capture after Seeing off.
        cancelled.store(true, Ordering::Release);
        screen_lane(app, move || { if let Ok(Ok(s)) = rx.recv() { drop(s); } });
    }
}
/// Runs a screen start or stop on its own thread, after the one before it. Dropping a stream joins its owner, which
/// first waits for any frame callback in flight, and a callback's archive write waits for the archive lock behind a
/// compaction step (24 Oct 02:31: Seeing off held every later click, and Quit, behind it). If the thread cannot
/// start, `work` is dropped here: a stream stops on the live loop as before, a start reports that its thread ended.
fn screen_lane(app: &mut pipeline::Pipeline, work: impl FnOnce() + Send + 'static) {
    let before = app.screen_lane.take();
    match std::thread::Builder::new().name("heimdall-screen-lane".into()).spawn(move || {
        if let Some(before) = before { let _ = before.join(); }
        work();
    }) {
        Ok(lane) => app.screen_lane = Some(lane),
        Err(e) => eprintln!("Screen thread did not start: {e}"),
    }
}
/// How long Seeing on waits on the live loop for its stream. A slower start (the buffer's first archive write waits
/// for the write lock behind a compaction step; ScreenCaptureKit may take seconds) keeps running on its own thread
/// and a later settings tick adopts it, so no click waits behind it.
pub(super) const SCREEN_START_WAIT: Duration = Duration::from_secs(2);
pub(super) fn start_screen(
    app: &mut pipeline::Pipeline,
    screens: &mut Option<screen::ScreenCapture>,
) -> Result<()> {
    if screens.is_some() || app.screen_starting.is_some() {
        return adopt_screen(app, screens, Duration::ZERO);
    }
    let disk=archive::DiskGuard {dir:app.session.parent().unwrap().to_path_buf(),floor:app.settings.free_space_floor_bytes,live_floor:Some(app.tools.archive_floor.clone()),status:app.disk_status.clone()};
    let guard=Arc::new(move |bytes:usize|disk.allows(bytes as u64));
    let (config, session) = (app.settings.screen.config(&ui::display_sizes()), app.session.clone());
    let started_with = format!("{config:?}");
    let (tx, rx) = bounded(1);
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancel = cancelled.clone();
    screen_lane(app, move || {
        if cancel.load(Ordering::Acquire) {
            let _ = tx.send(Err(anyhow::anyhow!("Seeing went off before the screen started")));
            return;
        }
        // Nobody waits any more (Seeing went off, or Quit): the stream stops here.
        if let Err(crossbeam_channel::SendError(Ok(s))) = tx.send(screen::ScreenCapture::start_recording(config, session, guard)) {
            drop(s);
        }
    });
    app.screen_starting = Some((rx, started_with, cancelled));
    adopt_screen(app, screens, SCREEN_START_WAIT)
}
/// Installs a stream that finished starting, waiting up to `wait` for it. A failed start is reported and returned.
/// A capture start that finished (`CaptureRun::begin`): adopted while listening still wants it, else stopped off the
/// loop.
pub(super) fn adopt_capture(app: &mut pipeline::Pipeline, capture: &mut Option<CaptureRun>, tx: &crossbeam_channel::Sender<capture::CaptureEvent>, started: Result<(capture::Capture, capture::CaptureReceiver)>, wanted: bool) {
    app.capture_starting = None;
    match started {
        Ok((c, rx)) if wanted && capture.is_none() => {
            app.capture_started();
            *capture = Some(CaptureRun::adopt(app, tx.clone(), c, rx));
            // It started with the settings of 10-30 s ago: a microphone or app chosen meanwhile (saved, with no capture
            // to tell) applies now (review 5 Nov: a USB mic picked during the start was lost).
            apply_microphones(app, capture, &None);
            apply_recorded_apps(app, capture, &None);
        }
        Ok(unwanted) => { std::thread::spawn(move || drop(unwanted)); }
        Err(e) => app.fail(&format!("Capture: {e:#}")),
    }
}

/// Listening went off while capture was starting: whatever it starts is stopped on its own thread.
fn drop_starting_capture(app: &mut pipeline::Pipeline) {
    if let Some(rx) = app.capture_starting.take() {
        std::thread::spawn(move || { if let Ok(Ok(started)) = rx.recv() { drop(started); } });
    }
}

pub(super) fn adopt_screen(app: &mut pipeline::Pipeline, screens: &mut Option<screen::ScreenCapture>, wait: Duration) -> Result<()> {
    let Some((rx, _, _)) = &app.screen_starting else { return Ok(()) };
    let started = match rx.recv_timeout(wait) {
        Ok(started) => started,
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => return Ok(()),
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(anyhow::anyhow!("the screen start thread ended")),
    };
    let (_, started_with, _) = app.screen_starting.take().expect("checked above");
    let s = match started {
        Ok(s) => s,
        Err(e) => {
            app.screen_event(screen::ScreenEvent::Error(format!("screen start failed: {e:#}")));
            return Err(e);
        }
    };
    // Screen Recording settings changed while it started.
    let now = app.settings.screen.config(&ui::display_sizes());
    if format!("{now:?}") != started_with {
        if let Err(e) = s.reconfigure(now) { app.settings_result("Screen Recording", Some(format!("{e:#}"))); }
    }
    app.screen_buffer = s.buffer.clone();
    *app.tools.buffer.lock().unwrap() = Some(s.buffer.clone());
    *screens = Some(s);
    Ok(())
}

/// Applies every Vella notification already queued, so a start, stop or heartbeat that arrived before a final (or before
/// the timeout check) counts first: `select!` picks among ready channels at random (vella.rs, docs/vella.md).
pub(super) fn drain_vella(app: &mut pipeline::Pipeline, rx: &crossbeam_channel::Receiver<crate::vella::Heard>) {
    while let Ok(heard) = rx.try_recv() {
        app.vella_notice(heard);
    }
}

#[cfg(test)]
mod off_loop_tests {
    use super::*;

    /// VM run 5 Nov: capture's HAL discovery waited 10-30 s on the live loop and the watchdog relaunched Heimdall
    /// mid-Setup. A start handed to `off_loop` returns at once; its result arrives later on the channel.
    #[test]
    fn a_slow_start_never_holds_the_caller() {
        let started = Instant::now();
        let rx = off_loop("slow-start", || { std::thread::sleep(Duration::from_millis(600)); Ok(7) });
        assert!(started.elapsed() < Duration::from_millis(100), "{:?}", started.elapsed());
        assert!(rx.try_recv().is_err(), "not yet");
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap(), 7);
        let failed = off_loop("failing-start", || Err::<(), _>(anyhow::anyhow!("HAL discovery startup failed")));
        assert!(failed.recv_timeout(Duration::from_secs(5)).unwrap().is_err());
    }
}
