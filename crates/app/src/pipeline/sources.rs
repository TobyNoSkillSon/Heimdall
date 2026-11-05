//! Capture sources and listen workers: running/paused, source arrival and removal, microphones, the live worker's
//! lifecycle and events, Discord relabelling, gaps.
use super::*;

/// Transcription stopped on its own three times in ten minutes: Heimdall relaunches (`worker_ended`).
pub(super) const TRANSCRIPTION_GAVE_UP: &str = "Listen failed: transcription stopped 3 times in 10 minutes";

impl Pipeline {
    /// The live 2-second tick's call work: queued speech-split corrections, then each call's transcript projection.
    pub(crate) fn call_tick(&mut self) -> Result<()> {
        self.correction_tick()?;
        let calls: Vec<_> = self.discord.keys().cloned().collect();
        for source in calls {
            self.project_call(&source)?;
        }
        Ok(())
    }
    /// Listening on/off. Off records an explicit chosen gap and unloads the decider (it has nothing to decide);
    /// the agent, speech and the screen keep running: each switch turns off only itself.
    pub fn set_running(&mut self, on: bool, reason: &str) -> Result<()> {
        let host_now_ns = capture::host_now_ns();
        self.index.event(
            if on { "heimdall_on" } else { "heimdall_off" },
            "heimdall",
            host_now_ns,
            &serde_json::json!({ "reason": reason }),
        )?;
        if !on {
            self.health_off();
            self.listening_since = None;
            self.send_worker.take();
            self.decider_unloaded();
            self.command_inflight.clear();
            let deferred=std::mem::take(&mut self.command_deferred);
            self.command_candidates.clear();
            for (_,events) in deferred {for event in events {
                if let ListenEvent::FinalUtterance {source,words,start_ns,end_ns,..}=&event {
                    let text=listen::Transcript::from_words(words.clone()).text;
                    self.index.evidence.event("deferred_final_cancelled",source,&[],*end_ns,serde_json::json!({"text":text,"start_ns":start_ns,"reason":"listening_off; retained in listen_event"}))?;
                }
            }}
            self.early_sends.clear();self.early_ids.clear();
            self.barge = None;
        }
        Ok(())
    }
    /// Listening off after capture stopped: every source closes and the decider unloads. The worker and its models
    /// stay loaded (no CPU or wakeups while idle): the next On skips a ~2.3 s load. The screen, the agent and speech
    /// are not touched: each switch turns off only itself.
    pub fn stop_listening(&mut self, rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>) -> Result<()> {
        self.close_live_sources(rx)?;
        self.pause_sources();
        self.capture_since = None;
        self.set_running(false, "setting")
    }
    /// Every open source ends with a `gap` (reason "heimdall off"). A chosen pause is not a fault, so the
    /// orange "Audio gap recorded" status it would raise is cleared.
    /// Capture opened: a held "Capture:" failure from an earlier attempt is over.
    pub fn capture_started(&mut self) {
        self.component_status("capture", None);
        self.capture_since = Some(Instant::now());
    }
    pub fn pause_sources(&mut self) {
        self.component_status("capture", None);
        let ids: Vec<_> = self.sources.keys().cloned().collect();
        for id in &ids {
            self.gap(id, capture::host_now_ns(), "heimdall off");
            self.component_status(&format!("gap:{id}"), None);
        }
        self.sources.clear();
        self.sources_ui();
    }
    pub fn raw_archive(&self) -> Result<Box<dyn capture::ArchiveSink>> {
        Ok(Box::new(RawArchive::new(
            &self.session,
            &self.settings,
            self.disk_status.clone(),
            self.enabled.clone(),
        )?.with_floor_source(self.tools.archive_floor.clone())))
    }
    pub fn source(&mut self, i: capture::SourceInfo) {
        // Capture taps only what the user chose (App Audio) plus his mics and Heimdall itself: every source is on.
        self.enabled.lock().unwrap().insert(i.id.0.clone(), true);
        if let Err(e) = self.index.evidence.event(
            "source",
            &i.id.0,
            &[],
            capture::host_now_ns(),
            serde_json::to_value(&i).unwrap(),
        ) {
            self.fail(&format!("Source archival failed: {e}"));
        }
        self.sources.insert(i.id.0.clone(), i);
        self.sources_ui();
    }
    /// The status line's sources: one per mic and per recorded app (its helpers fold into it), by app name.
    /// Heimdall's own output is archived but not listed: it is not something the user chose to hear.
    pub(super) fn sources_ui(&self) {
        let mut out: Vec<ui::Source> = vec![];
        for i in self.sources.values() {
            if self.own_audio(&i.id.0) {
                continue;
            }
            let (kind, name) = match &i.kind {
                capture::SourceKind::InputDevice { .. } => (ui::SourceKind::Mic, i.label.clone()),
                capture::SourceKind::Process { name, owner, .. } => {
                    (ui::SourceKind::App, owner.as_ref().map_or(name.clone(), |o| o.name.clone()))
                }
            };
            if kind == ui::SourceKind::App && out.iter().any(|s| s.kind == kind && s.name == name) {
                continue;
            }
            out.push(ui::Source { id: i.id.0.clone(), kind, name, enabled: true });
        }
        out.sort_by(|a, b| (a.kind != ui::SourceKind::Mic, &a.name).cmp(&(b.kind != ui::SourceKind::Mic, &b.name)));
        self.ui(ui::UiUpdate::Sources(out));
    }
    pub(super) fn owner(&self, id: &str) -> bool {
        self.sources
            .get(id)
            .is_some_and(|i| matches!(i.kind, capture::SourceKind::InputDevice { .. }))
    }
    /// Capture's effective primary includes the built-in fallback, regardless of saved extras.
    pub fn microphone_state(&mut self, state: &capture::MicrophoneState) {
        if self.effective_primary_uid != state.in_use {
            let old: Vec<_> = self.sources.keys().filter(|id| self.primary_mic(id)).cloned().collect();
            for id in old { self.reset_commands(&id); }
        }
        self.effective_primary_uid = state.in_use.clone();
        self.primary_reported = true;
        self.mic_health(state);
    }
    pub(super) fn primary_mic(&self, id: &str) -> bool {
        if (self.primary_reported && self.effective_primary_uid.is_none()) || (!self.replay && !self.primary_reported) {return false;}
        let selected = self.effective_primary_uid.as_deref().or_else(|| {
            self.settings.microphone.as_ref().map(|m| m.uid.as_str()).filter(|uid| {
                self.sources.values().any(|i| matches!(&i.kind, capture::SourceKind::InputDevice { uid: active, .. } if active == uid))
            })
        });
        self.sources.get(id).is_some_and(|i| match &i.kind {
            capture::SourceKind::InputDevice { uid, .. } => selected.map_or_else(
                // Permission-free replay has no HAL report. Its first non-extra input is primary.
                || !self.settings.extra_microphones.iter().any(|m| &m.uid == uid),
                |primary| primary == uid),
            _ => false,
        })
    }
    pub(super) fn own_audio(&self, id: &str) -> bool {
        self.sources.get(id).is_some_and(|i|matches!(&i.kind,capture::SourceKind::Process{pid,bundle_id,..} if *pid==std::process::id() || bundle_id.as_deref()==Some("dev.heimdall.app")))
    }
    pub(super) fn is_call(&self, id: &str) -> bool {
        self.sources.get(id).is_some_and(crate::context_policy::is_call)
    }
    pub fn enable_live_worker(
        &mut self,
        tx: crossbeam_channel::Sender<Option<listen::WorkerEvent>>,
    ) -> Result<()> {
        if let Some(stop) = self.live_stop.take() {
            stop.store(true, Ordering::Release);
        }
        let models = self.models.clone();
        let evidence = self.evidence.clone();
        let settings = self.listen_settings.clone();
        let initial=settings.lock().unwrap().clone();
        let mut applied=initial.clone();
        let reload_models=models.clone();
        let worker = Arc::new(listen::ListenWorker::spawn_configurable(
            // The integration swap replay exposed a 14.02-second cold-backend
            // backlog on continuous speech. Cover it with 15.36 seconds at 20 ms;
            // 768 x 1600 x 4 bytes x 32 sources is at most 150 MiB.
            listen::QueueConfig {
                chunks_per_source: 768,
                ..Default::default()
            },
            move || {
                let s = initial;
                let asr = listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), &s.parakeet_backend)?;
                let mut engine = ListenEngine::load_with_turn_backend(&models, asr, Some(&s.turn_backend))?;
                install_listen_evidence(&mut engine, evidence.clone());
                Ok(engine)
            },
            move |engine| {
                let next=settings.lock().unwrap().clone();
                let asr=if next.parakeet_backend!=applied.parakeet_backend {
                    Some(listen::asr::Parakeet::load_backend(&reload_models.join("parakeet-ultra"),&next.parakeet_backend)?)
                }else{None};
                let turn=if next.turn_backend!=applied.turn_backend {
                    let turn_path=if reload_models.join("smart-turn/owner-policy.json").exists(){"smart-turn/owner-turn.onnx"}else{"smart-turn/smart-turn-v3.2-cpu.onnx"};
                    Some(listen::onnx::SmartTurn::load_backend(&reload_models.join(turn_path),&next.turn_backend)?)
                }else{None};
                // Construct both requested candidates before replacing either model.
                if let Some(asr)=asr{engine.replace_recognizer(asr);}
                if let Some(turn)=turn{engine.replace_turn_detector(turn);}
                applied=next;
                Ok(())
            },
        )?);
        let output = worker.clone();
        let tools = self.tools.clone();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stop.clone();
        let night_activity = self.night.as_ref().map(|n| n.activity());
        std::thread::spawn(move || {
            while let Some(event) = output.recv_event() {
                if let Some(night) = &night_activity {
                    match &event {
                        listen::WorkerEvent::Listen(ListenEvent::SpeechStart { source, .. }) => night.speech(source, true),
                        listen::WorkerEvent::Listen(ListenEvent::SpeechEnd { source, .. }) | listen::WorkerEvent::Gap { source, .. } | listen::WorkerEvent::SourceClosed { source } => night.speech(source, false),
                        _ => {}
                    }
                }
                match &event {
                    listen::WorkerEvent::Listen(ListenEvent::SpeechStart { source, .. })
                        if source.starts_with("mic:") =>
                    {
                        // Heard now; whether Heimdall stops is the barge-in decision (pipeline/barge_in.rs).
                        tools.voice(source, true);
                    }
                    listen::WorkerEvent::Listen(ListenEvent::SpeechEnd { source, .. })
                    | listen::WorkerEvent::Gap { source, .. }
                    | listen::WorkerEvent::SourceClosed { source } => tools.voice(source, false),
                    _ => {}
                }
                // An abandoned (hung) worker that moves again talks to nobody.
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                let mut e = Some(event);
                loop {
                    match tx.send_timeout(e, std::time::Duration::from_millis(100)) {
                        Ok(()) => break,
                        Err(crossbeam_channel::SendTimeoutError::Timeout(event)) => {
                            if stopped.load(Ordering::Acquire) {
                                return;
                            }
                            e = event;
                        }
                        Err(_) => return,
                    }
                }
            }
            let _ = tx.send(None);
        });
        self.correction_worker_ended();
        self.live_worker = Some(worker);
        self.live_stop = Some(stop);
        self.worker_spawned = Some(Instant::now());
        self.worker_watch = None;
        Ok(())
    }
    pub(crate) fn record_teardown_error(&self, error: &str) {
        if let Err(e) = crate::watchdog::RecoveryLog::record_error(&self.data, error) {
            eprintln!("Could not save teardown error: {e:#}");
        }
    }
    pub fn stop_live_worker(
        &mut self,
        rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>,
    ) -> Result<()> {
        if let Some(worker) = self.live_worker.clone() {
            worker.request_shutdown();
            self.drain_worker_events(rx, Some(&worker));
        }
        if let Some(stop) = self.live_stop.take() {
            stop.store(true, Ordering::Release);
        }
        self.live_worker.take();
        Ok(())
    }
    /// Drains until the forwarder's end marker. Bounded: if the worker already ended and its marker
    /// was consumed by the live loop, waiting for a second one hung Heimdall's loop for good (the user,
    /// 13 Oct 11:16: Listening off froze every later click and Quit).
    pub(super) fn drain_worker_events(&mut self, rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>, worker: Option<&listen::ListenWorker>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            // Bounded by its own deadline: Quit's watchdog lets it finish the last finals.
            crate::watchdog::progress();
            let next = match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(next) => Ok(next),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    let finished = worker.is_none_or(|w| w.is_finished());
                    if (finished && rx.is_empty()) || std::time::Instant::now() >= deadline {
                        if !finished {
                            self.errors += 1;
                            let error = "Listen worker did not finish within 20 s of shutdown";
                            self.record_teardown_error(error);
                            eprintln!("{error}; continuing");
                        }
                        break;
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(crossbeam_channel::RecvError),
            };
            match next {
                Ok(Some(event)) => if let Err(e) = self.worker_event(event) {
                    self.errors += 1;
                    self.record_teardown_error(&format!("Listen event failed during shutdown: {e:#}"));
                    eprintln!("Listen event failed during shutdown; continuing drain: {e:#}");
                    self.component_status("listen", Some(format!("Final processing failed: {e:#}")));
                },
                Ok(None) => break,
                Err(e) => {self.errors += 1;self.record_teardown_error(&format!("Listen output disconnected during shutdown: {e}"));eprintln!("Listen output disconnected during shutdown: {e}");break;},
            }
        }
    }
    pub(super) fn inference(&mut self) -> Result<&send_worker::InferenceOwner> {
        if self.send_worker.is_none() {
            self.send_worker = Some(send_worker::InferenceOwner::start(
                self.models.join("decide"),
                archive::run_dir(&self.data, &self.session).join("decisions"),
                self.session.clone(),
                self.evidence.clone(),
                self.internal.0.clone(),
                self.decider_pick(),
            )?);
        }
        Ok(self.send_worker.as_ref().unwrap())
    }
    pub fn preload(
        &mut self,
        tx: crossbeam_channel::Sender<Option<listen::WorkerEvent>>,
        _speakers: bool,
    ) -> Result<()> {
        self.enable_live_worker(tx)?;
        self.live_worker.as_ref().unwrap().wait_ready()?;
        self.prepare_decisions()
    }
    /// Decisions (unloaded at Listening off) and the agent. Listening on calls this after capture has started.
    pub fn prepare_decisions(&mut self) -> Result<()> {
        // A decider that does not load (it retries on its own, with health on the island) never keeps the agent
        // from starting.
        if self.decider_phase != decider::Phase::Ready { self.decider_phase(decider::Phase::Loading); }
        let decider = self.inference().and_then(|owner|owner.request(2,0,|s|{let _=s.decider();},|_,_|Ok(())));
        self.ensure_agent()?;
        decider
    }
    /// The agent runtime, started if there is none. It runs whatever Listening says: launched with Listening off,
    /// it still takes notices, wakes and typed messages.
    pub fn ensure_agent(&mut self) -> Result<()> {
        if self.agent.is_none() {
            self.agent = Some(AgentRuntime::start(
                self.settings.clone(),
                self.data.clone(),
                self.session.clone(),
                self.tools.clone(),
                self.ui,
            )?);
            self.agent.as_ref().unwrap().barrier()?;
            self.greet_if_due();
        }
        Ok(())
    }
    /// The listen worker, started if there is none or the last one ended. Never waits for its models: they load on
    /// the worker's own thread while audio queues there (15 s), so Listening on starts capture at once.
    pub fn ensure_live_worker(&mut self, tx: crossbeam_channel::Sender<Option<listen::WorkerEvent>>) -> Result<()> {
        if self.live_worker_running() { return Ok(()); }
        self.enable_live_worker(tx)
    }
    /// The current listen worker, lent to Vella's transcription socket (vella_asr.rs).
    pub fn live_worker(&self) -> Option<Arc<listen::ListenWorker>> { self.live_worker.clone() }
    pub fn live_worker_running(&self) -> bool {
        self.live_worker.as_ref().is_some_and(|w| !w.is_finished())
    }
    /// Listening off with the models kept loaded (measured idle cost: no CPU, no wakeups; about 2 GB of memory).
    /// Every open source's last words become finals, as at a stop, and its slot closes so it can return.
    pub fn close_live_sources(&mut self, rx: &crossbeam_channel::Receiver<Option<listen::WorkerEvent>>) -> Result<()> {
        let Some(worker) = self.live_worker.clone() else { return Ok(()) };
        let mut open: std::collections::HashSet<String> = worker.close_all_sources().into_iter().collect();
        let deadline = Instant::now() + std::time::Duration::from_secs(20);
        while !open.is_empty() {
            match rx.recv_timeout(std::time::Duration::from_millis(250)) {
                Ok(Some(event)) => {
                    if let listen::WorkerEvent::SourceClosed { source } = &event { open.remove(source); }
                    if let Err(e) = self.worker_event(event) {
                        self.errors += 1;
                        eprintln!("Listen event failed while closing sources; continuing: {e:#}");
                    }
                }
                Ok(None) => {
                    self.worker_ended(None)?;
                    if self.live_worker.is_none() { break; }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    if worker.is_finished() && rx.is_empty() { break; }
                    if Instant::now() >= deadline {
                        self.errors += 1;
                        eprintln!("Listen sources did not close within 20 s of Listening off; continuing");
                        break;
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        Ok(())
    }
    /// Listening on was asked for: the island shows the Listening glyph until the models answer.
    pub fn listening_starting(&mut self) {
        self.health_listening();
        self.listening_since = Some(Instant::now());
        self.ui(ui::UiUpdate::Command(ui::CommandCue::Heard { id: LISTEN_CUE, kind: ui::CommandKind::Listening }));
    }
    /// Listening on without any audio permission starts nothing: the glyph leaves (the menu shows Set Up).
    pub fn listening_not_starting(&mut self) {
        self.listening = false;
        if self.listening_since.take().is_some() {
            self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: LISTEN_CUE }));
        }
    }
    /// "Listening" on the island once the worker's models are loaded (at once when they stayed loaded), with the
    /// time from the click recorded.
    pub fn listening_ready_check(&mut self) {
        let Some(since) = self.listening_since else { return };
        if !self.live_worker.as_ref().is_some_and(|w| w.is_ready() && !w.is_finished()) { return; }
        self.listening_since = None;
        let ms = since.elapsed().as_secs_f64() * 1000.;
        if let Err(e) = self.index.event("listening_ready", "heimdall", capture::host_now_ns(), &serde_json::json!({"ms_since_on": ms})) {
            eprintln!("listening_ready not archived: {e:#}");
        }
        self.ui(ui::UiUpdate::Command(ui::CommandCue::Done { id: Some(LISTEN_CUE), kind: ui::CommandKind::Listening, label: "Listening".into() }));
    }
    /// The forwarder's end marker reached the loop outside a stop: the worker ended on its own (its reason came
    /// first as an Error event). Recorded and shown; while listening it restarts, at most 3 times in 10 minutes.
    /// A marker from an earlier worker (the current one still running) is ignored.
    pub fn worker_ended(&mut self, restart: Option<&crossbeam_channel::Sender<Option<listen::WorkerEvent>>>) -> Result<()> {
        if !self.live_worker.as_ref().is_some_and(|w| w.is_finished()) { return Ok(()); }
        self.correction_worker_ended();
        self.live_worker = None;
        if let Some(stop) = self.live_stop.take() {
            stop.store(true, Ordering::Release);
        }
        // Utterances in flight lost their tail with the worker.
        let ids: Vec<_> = self.sources.keys().cloned().collect();
        for id in &ids {
            self.gap(id, capture::host_now_ns(), "transcription restarted");
        }
        let now = Instant::now();
        while self.worker_restarts.front().is_some_and(|t| now.duration_since(*t) > std::time::Duration::from_secs(600)) {
            self.worker_restarts.pop_front();
        }
        let restarting = restart.is_some() && self.worker_restarts.len() < 3;
        self.index.event("listen_worker_ended", "heimdall", capture::host_now_ns(),
            &serde_json::json!({"restarting": restarting, "listening": restart.is_some(), "last_failure": self.failure}))?;
        let Some(tx) = restart else { return Ok(()) };
        if restarting {
            // Orange "Restarting transcription…" until the new worker is ready ("Transcription OK").
            self.worker_restarts.push_back(now);
            self.note_line("Transcription stopped unexpectedly; restarting it");
            if self.listening_since.take().is_some() {
                self.ui(ui::UiUpdate::Command(ui::CommandCue::Dropped { id: LISTEN_CUE }));
            }
            let cue = self.health.restarting("listen", ui::Part::Transcription, "Restarting transcription…");
            self.health_cue(cue);
            self.enable_live_worker(tx.clone())?;
        } else {
            // The part's restarts are used up: the next rung relaunches Heimdall (main.rs decides; past its own
            // bound it shows this as a red problem).
            self.listening_since = None;
            self.fail(TRANSCRIPTION_GAVE_UP);
            self.request_recovery(crate::health::Recovery::Relaunch {
                key: "listen".into(),
                part: ui::Part::Transcription,
                problem: "Transcription keeps stopping".into(),
            });
        }
        Ok(())
    }
    pub fn worker_event(&mut self, event: listen::WorkerEvent) -> Result<()> {
        match event {
            listen::WorkerEvent::Ready => {
                self.component_status("listen", None);
                let cue = self.health.recovered("listen", ui::Part::Transcription, "Transcription OK");
                self.health_cue(cue);
                self.listening_ready_check();
                Ok(())
            }
            listen::WorkerEvent::ModelsReloaded { error } => {
                self.settings_result("Transcription Backend", error);
                Ok(())
            },
            listen::WorkerEvent::Listen(event) => {
                self.component_status("listen", None);
                self.listen(event)
            },
            listen::WorkerEvent::Gap {
                source,
                start_ns,
                end_ns,
                reason,
            } => {
                if !self.sources.contains_key(&source) || self.source_cutoffs.get(&source).is_some_and(|cutoff| end_ns <= *cutoff) {
                    self.index.evidence.event("obsolete_worker_event", &source, &[], start_ns,
                        serde_json::json!({"kind":"gap","start_ns":start_ns,"end_ns":end_ns,"reason":reason,"cutoff_ns":self.source_cutoffs.get(&source)}))?;
                } else {
                    self.gap(&source, end_ns, &format!("listen loss from {start_ns} to {end_ns}: {reason:?}"));
                }
                Ok(())
            }
            listen::WorkerEvent::Error { source, message } => {
                self.errors += 1;
                if let Some(id) = source {
                    self.gap(&id, capture::host_now_ns(), &message);
                }
                self.fail(&format!("Listen failed: {message}"));
                Ok(())
            }
            listen::WorkerEvent::SideTranscript { id, result, decode_ms, cpu_seconds } => {
                if self.correction_transcript(id,result.clone(),decode_ms,cpu_seconds)? {Ok(())}
                else {self.side_transcript(id,result,decode_ms)}
            },
            listen::WorkerEvent::SourceClosed { source } => {
                if let Some(d) = self.discord.get_mut(&source) {
                    if !d.closed {
                        d.actor.send(crate::speakers_runtime::Work::Finish)?;
                        d.closed = true;
                    }
                } else {
                    self.close_source(&source);
                }
                Ok(())
            }
        }
    }
    pub(super) fn engine(&mut self) -> Result<&mut ListenEngine<listen::asr::Parakeet>> {
        if self.engine.is_none() {
            let asr = listen::asr::Parakeet::load_backend(&self.models.join("parakeet-ultra"), &self.settings.parakeet_backend)?;
            let mut engine = ListenEngine::load_with_turn_backend(&self.models, asr, Some(&self.settings.turn_backend))?;
            install_listen_evidence(&mut engine, self.evidence.clone());
            self.engine = Some(engine);
        }
        Ok(self.engine.as_mut().unwrap())
    }
    pub(super) fn start_discord(&mut self, id: &str, speech_ns: u64) -> Result<()> {
        if let Some(d) = self.discord.get_mut(id) {
            if d.active || d.loading {
                d.speaking = true;
                return Ok(());
            }
        }
        ensure!(
            self.discord.len() < 8 || self.discord.contains_key(id),
            "Call source bound reached"
        );
        let Some(ring) = self.audio.get(id) else {
            return Ok(());
        }; // A reported gap can precede an older queued VAD event.
        let start = speech_ns.saturating_sub(250_000_000).max(ring.start);
        let skip = (start.saturating_sub(ring.start) / 62500) as usize;
        let origin = ring.start + skip as u64 * 62500;
        let audio: Vec<_> = ring.samples.iter().skip(skip).copied().collect();
        let fed_until = origin + audio.len() as u64 * 62500;
        let now = Instant::now();
        if let Some(d) = self.discord.get_mut(id) {
            d.actor
                .send(crate::speakers_runtime::Work::Activate(origin, now))?;
            d.origin = origin;
            d.session = origin;
            d.active = true;
            d.loading = true;
            d.speaking = true;
            d.segments.clear();
            d.actor
                .send(crate::speakers_runtime::Work::Audio(audio, true, now))?;
            d.fed_until = fed_until;
        } else {
            let actor = crate::speakers_runtime::Actor::start(
                id.into(),
                origin,
                self.models.join("speakers"),
                self.session.clone(),
                NamingFlow::new(self.load_store()?).persist_to(self.data.join("speakers.json"), self.enrollment_lock.clone()),
                self.speaker_cpu.clone(),
                self.speaker_tx.clone(),
                self.settings.speakers_backend.clone(),
            )?;
            actor.send(crate::speakers_runtime::Work::Audio(audio, true, now))?;
            self.discord.insert(
                id.into(),
                Discord {
                    origin,
                    session: origin,
                    actor_generation: origin,
                    actor,
                    speaking: true,
                    fed_until,
                    active: true,
                    loading: true,
                    closed: false,
                    segments: vec![],
                    unlabelled: vec![],
                },
            );
        }
        crate::diagnostics::event_stream(&serde_json::json!({"event":"speaker_helper_loading","source":id}));
        Ok(())
    }
    pub fn source_open(&self, id: &str) -> bool {
        self.sources.contains_key(id)
    }
    pub fn speaker_ready(&mut self, event: crate::speakers_runtime::Event) -> Result<()> {
        use crate::speakers_runtime::Update;
        let source = event.source;
        if !self
            .discord
            .get(&source)
            .is_some_and(|d| d.actor_generation == event.generation)
        {
            return Ok(());
        }
        match event.update {
            Update::Stable { origin, until, segments, finalized } => {
                self.stable_speech(&source,origin,until,segments,finalized)?;
            }
            Update::Started { origin } => {
                let d = self.discord.get_mut(&source).unwrap();
                d.origin = origin; d.session = origin; d.loading = false; d.active = true;
                d.segments.clear();
                self.component_status(&format!("speakers:{source}"), None);
                self.component_status("speakers", None);
                crate::diagnostics::event_stream(&serde_json::json!({"event":"speaker_helper_ready","source":source}));
            }
            Update::Snapshot {
                origin,
                segments,
                events,
            }
            | Update::Closed {
                origin,
                segments,
                events,
            }
            | Update::Expired {
                origin,
                segments,
                events,
            } => {
                let d = self.discord.get_mut(&source).unwrap();
                if d.origin == origin {
                    d.segments = segments;
                }
                self.speaker_events(events)?;
                self.relabel_discord(&source)?;
            }
            Update::GpuStatus(status) => {
                // Teardown's empty GPU status cannot clear a helper outage.
                if self.discord[&source].active {
                    self.component_status(
                        &format!("speakers:{source}"),
                        status.map(|_| "Speakers using CPU fallback".into()),
                    );
                }
            }
            Update::Error(error) => {
                // This is a labelling outage, not an audio discontinuity. Keep
                // the listen engine, waveform ring and utterance prefix intact.
                let d = self.discord.get_mut(&source).unwrap();
                d.active = false;
                d.loading = true; // actor owns retry; intake must not spam Activate.
                d.segments.clear();
                let closed = d.closed;
                self.component_status(&format!("speakers:{source}"), Some(error.split(": ").next().unwrap_or("Speaker ? · retrying helper").into()));
                self.index.evidence.event("speaker_helper_failure", &source, &[], capture::host_now_ns(), serde_json::json!({"error":error,"asr_preserved":true}))?;
                if closed { self.close_source(&source); }
            }
        }
        Ok(())
    }
    pub fn speaker_update(&mut self, event: crate::speakers_runtime::Event) -> Result<()> {
        use crate::speakers_runtime::Update;
        let source = event.source.clone();
        let close = matches!(&event.update, Update::Closed { .. });
        let expire = match &event.update {
            Update::Expired { origin, .. } => self
                .discord
                .get(&source)
                .is_some_and(|d| d.origin == *origin),
            _ => false,
        };
        let accepted = self
            .discord
            .get(&source)
            .is_some_and(|d| d.actor_generation == event.generation);
        self.speaker_ready(event)?;
        if accepted && (close || expire) {
            if let Some(d)=self.discord.get(&source) {
                let origin=d.origin;let segments=d.segments.clone();
                let end=segments.iter().map(|s|origin+(s.end_time*1e9).round() as u64).max().unwrap_or(origin);
                // At source close/quiet expiry the final window may have less than
                // one second of right context. Own existing audio only.
                self.stable_speech(&source,origin,end.saturating_add(1_000_000_000),segments,false)?;
                self.correction_end(&source,origin);
            }
            self.correction_tick()?;
            self.component_status(&format!("speakers:{source}"), None);
        }
        if accepted && close {
            self.close_source(&source);
        } else if accepted && expire {
            if let Some(d) = self.discord.get_mut(&source) {
                d.active = false;
                d.loading = false;
            }
            crate::diagnostics::event_stream(&serde_json::json!({"event":"speaker_helper_expired","source":source}));
        }
        Ok(())
    }
    pub(super) fn relabel_discord(&mut self, source: &str) -> Result<()> {
        if let Some(d)=self.discord.get(source) {
            // The snapshot route owns words; legacy whole-final labels only service
            // finals that have no per-word projection.
            let origin=d.origin;let segments=d.segments.clone();
            self.update_call_attribution(source,origin,segments);
        }
        let d = self.discord.get_mut(source).unwrap();
        let earliest = d.origin + (d.segments.first().map_or(0., |s| s.start_time) * 1e9) as u64;
        d.unlabelled.retain(|(_, _, end, _, _)| *end > earliest);
        let pending = std::mem::take(&mut d.unlabelled);
        let attributed=self.call_final_ids();
        for (id, start, end, text, old) in pending {
            if let Some(track) = self.track_for(source, start, end)? {
                let name = self
                    .speaker_names
                    .get(&track)
                    .cloned()
                    .unwrap_or_else(|| format!("Speaker {}", track.speaker));
                if let Some((line, saved_track)) = self.labels.get_mut(&id) {
                    line.label.as_mut().unwrap().speaker = name.clone();
                    *saved_track = Some(track.clone());
                }
                if !attributed.contains(&id) {
                if let Some((line, _)) = self.labels.get(&id) {
                    self.ui(ui::UiUpdate::Transcript(line.clone()));
                }
                self.index.relabel(
                    source,
                    &[format!(
                        "{}:{}",
                        self.session.file_name().unwrap().to_string_lossy(),
                        id
                    )],
                    &name,
                )?;
                }
                if !self.discord[source].closed {
                    self.discord.get(source).unwrap().actor.send(
                        crate::speakers_runtime::Work::Observe(
                            track,
                            end as f64 / 1e9,
                            text.clone(),
                            vec![],
                        ),
                    )?;
                }
                if let Some(mut final_) = self.awaiting_speaker.remove(&id) {
                    final_.observation.speaker = name.clone();
                    final_.turn.role = decide::Role::Other(name.clone());
                    self.give_to_agent(final_)?;
                }
                crate::diagnostics::event_stream(&serde_json::json!({"event":"speaker_label","utterance_id":id,"source":source,"speaker":name,"previous":old}));
            } else {
                self.discord
                    .get_mut(source)
                    .unwrap()
                    .unlabelled
                    .push((id, start, end, text, old));
            }
        }
        Ok(())
    }
    pub(super) fn close_source(&mut self, source: &str) {
        self.correction_end(source,0);
        let _=self.correction_tick();
        self.sources.remove(source);
        self.drops.forget(source);
        self.enabled.lock().unwrap().remove(source);
        self.source_cutoffs.remove(source);
        self.active.remove(source);
        self.quiet_since.remove(source);
        self.tools.speech_policy.other_voice(source, None);
        self.reset_commands(source);
        self.audio.remove(source);
        self.archive_waiting_calls(source, "source_closed_before_diarization");
        self.discord.remove(source);
        self.speaker_names.retain(|t, _| t.source != source);
        self.component_status(&format!("gap:{source}"), None);
        self.sources_ui();
    }
    pub fn capture_event(&mut self, event: capture::CaptureEvent) -> Result<()> {
        match event {
            capture::CaptureEvent::SourceAdded(i) => {
                self.mic_source(&i, true);
                // A route change takes a device out and puts it back a moment later under a new generation: without a
                // line for the return, the log shows a live mic as gone (bug note 2026-10-27T00-47).
                let now = capture::host_now_ns();
                if let Some(at) = self.removed_sources.remove(device_key(&i.id.0)) {
                    let after_ms = now.saturating_sub(at) / 1_000_000;
                    if after_ms <= READDED_MS {
                        let _ = self.index.event("source_readded", &i.id.0, now, &serde_json::json!({"event":format!("back {after_ms} ms after its removal, generation {}", i.generation),"after_ms":after_ms,"generation":i.generation}));
                    }
                }
                let id = i.id.0.clone();
                self.source(i);
                // A call can be running again (bridge/speech_policy.rs, `call_source_removed`).
                if self.is_call(&id) { self.tools.speech_policy.call_source_added(); }
            }
            capture::CaptureEvent::SourceRemoved { source_id, host_ns, reason } => {
                let now = capture::host_now_ns().max(host_ns);
                // Only a quick return is logged (`READDED_MS`): a source that never comes back is forgotten, so app-audio
                // sources that come and go cannot grow this map for the life of the app (night review).
                self.removed_sources.retain(|_, at| now.saturating_sub(*at) / 1_000_000 <= READDED_MS);
                self.removed_sources.insert(device_key(&source_id.0).to_owned(), now);
                self.mic_audio.remove(&source_id.0);
                self.index.event("source_removed", &source_id.0, host_ns, &serde_json::json!({"reason":reason}))?;
                // The last call source stopped: the call ends a few seconds later unless one comes back (a route change
                // returns it within ~100 ms). Bug 2026-11-04T17-48-24.
                if self.is_call(&source_id.0) {
                    let left = self.sources.keys().any(|k| k != &source_id.0 && self.is_call(k));
                    let stopped = reason.starts_with("source stopped");
                    self.tools.speech_policy.call_source_removed(&source_id.0, stopped, left, now);
                }
                self.surface_gap_notice(&source_id.0, &reason);
                self.finish_source(&source_id.0)?;
                if self.live_worker.is_none() {
                    self.sources.remove(&source_id.0);
                    self.sources_ui();
                }
            }
            capture::CaptureEvent::Diagnostic { host_ns, detail } => {
                self.evidence.diagnostic("capture_diagnostic", "capture", host_ns, serde_json::json!({"detail":detail}));
            }
            capture::CaptureEvent::Gap {
                source_id,
                host_ns,
                detail,
            } => {
                if detail.starts_with("archive pipeline stopped:") {
                    *self.disk_status.lock().unwrap() = Some(detail.clone());
                    self.fail(&detail);
                    self.archive_stopped(&detail);
                }
                if let Some(id) = source_id {
                    // Native loss is already durable. ASR finishes its pre-gap PCM
                    // before the discontinuous chunk emits the ordered listen gap.
                    if detail.starts_with("native overflow:") || detail.starts_with("native clock discontinuity") || detail.starts_with("listen receiver lagged;") {
                        self.index.note("capture_gap", &id.0, host_ns, &serde_json::json!({"reason":detail}))?;
                        self.component_status(&format!("gap:{}", id.0), Some("Audio gap recorded".into()));
                    } else {
                        self.gap(&id.0, host_ns, &detail);
                    }
                } else {
                    let ids: Vec<_> = self.sources.keys().cloned().collect();
                    for id in ids {
                        self.gap(&id, host_ns, &detail);
                    }
                }
            }
            capture::CaptureEvent::Audio(c) => {
                // Capture delivers (whatever this loop then makes of the audio): the stall clock restarts.
                self.mic_audio_arrived(&c.source_id.0);
                if self
                    .source_cutoffs
                    .get(&c.source_id.0)
                    .is_some_and(|cutoff| c.host_ns < *cutoff)
                {
                    self.index.evidence.event("obsolete_capture_event", &c.source_id.0, &[], c.host_ns,
                        serde_json::json!({"kind":"audio","cutoff_ns":self.source_cutoffs.get(&c.source_id.0)}))?;
                    return Ok(());
                }
                if !self.sources.contains_key(&c.source_id.0) {
                    self.index.evidence.event("obsolete_capture_event", &c.source_id.0, &[], c.host_ns,
                        serde_json::json!({"kind":"audio","reason":"source_closed"}))?;
                    return Ok(());
                }
                let id = &c.source_id.0;
                // Audio flows again after a gap: the gap is recorded in the archive and over (23 Oct: a native
                // overflow held "Audio gap recorded" for hours, until the source closed).
                if !self.component_status.is_empty() {
                    let key = format!("gap:{id}");
                    if self.component_status.contains_key(&key) { self.component_status(&key, None); }
                }
                let generation = self.sources.get(id).map_or(0, |s| s.generation);
                let clock_gap = self.audio.get(id).is_some_and(|r| {
                    (r.start + r.samples.len() as u64 * 62500).abs_diff(c.host_ns) > 125000
                });
                let discontinuity = c.discontinuity || clock_gap;
                if discontinuity {
                    self.index.note("capture_discontinuity", id, c.host_ns, &serde_json::json!({"capture_flag":c.discontinuity,"clock_gap":clock_gap}))?;
                }
                self.audio
                    .entry(id.clone())
                    .or_insert_with(|| AudioRing {
                        start: c.host_ns,
                        samples: VecDeque::new(),
                    })
                    .push(c.host_ns, &c.samples);
                self.barge_audio(id)?;
                if self
                    .discord
                    .get(id)
                    .is_some_and(|d| d.speaking && !d.active && !d.loading)
                {
                    self.start_discord(id, c.host_ns)?;
                }
                // An expired actor blocks on control until the next SpeechStart.
                // That activation resets its clock from the retained ASR ring;
                // quiet PCM would only wake it, allocate, and be discarded.
                if let Some(d) = self.discord.get_mut(id).filter(|d| d.active || d.loading) {
                    let skip = (d.fed_until.saturating_sub(c.host_ns) / 62500) as usize;
                    if skip < c.samples.len() {
                        d.actor.send(crate::speakers_runtime::Work::Audio(
                            c.samples[skip..].to_vec(),
                            d.speaking,
                            Instant::now(),
                        ))?;
                        d.fed_until = c.host_ns + c.samples.len() as u64 * 62500;
                    }
                }
                if let Some(worker) = &self.live_worker {
                    // A worker that just ended is restarted when its end marker arrives (worker_ended); until
                    // then its audio is a gap, not one error per chunk.
                    if !worker.is_finished() {
                        worker.push_with_discontinuity(listen::AudioChunk {
                            source: id,
                            generation,
                            host_time_ns: c.host_ns,
                            samples: &c.samples,
                        }, discontinuity)?;
                    }
                } else if self.replay {
                    // Replay transcribes inline. Live audio never loads models on this loop: without a worker
                    // (it stopped repeatedly) it is archived by capture and not transcribed.
                    if discontinuity {
                        for event in self.engine()?.discontinue_source(id, c.host_ns)? { self.listen(event)?; }
                    }
                    let events = self.engine()?.push(listen::AudioChunk {
                        source: id,
                        generation,
                        host_time_ns: c.host_ns,
                        samples: &c.samples,
                    })?;
                    for event in events {
                        self.listen(event)?;
                    }
                }

            }
        }
        self.component_tick();
        Ok(())
    }
    /// The parts' health and Jarvis's residency. Runs after each capture event and on the live loop's settings tick,
    /// so it never depends on capture: with Listening off no capture event arrives, and Jarvis never loaded while
    /// Speaking was on (bug note 2026-10-30T18-42-08: replies came in the lighter voice).
    pub fn component_tick(&mut self) {
        let gpu = listen::gpu_recovery::gpu_status();
        self.component_status_reason("listen-gpu", gpu.map(|_| "Listening using fallback".into()), gpu);
        let gpu = decide::gpu_recovery::gpu_status();
        self.component_status_reason("decide-gpu", gpu.map(|_| "Decisions using CPU fallback".into()), gpu);
        let gpu = speak::gpu_recovery::gpu_status();
        self.component_status_reason("speak-gpu", gpu.map(|_| "Speech using CPU fallback".into()), gpu);
        let drama_wanted=!cfg!(test) && !self.replay &&
            jarvis_wanted(self.settings.speak_replies, self.settings.dry_run, &self.settings.drama3_residency, self.tools.speech_voices.route());
        // The Speed slider's sample is spoken by Jarvis only: he stays resident for it whatever Speaking says
        // (the voice's health follows the usual residency).
        self.tools.drama3.configure(drama_wanted || (!cfg!(test) && !self.replay && self.tools.sample_holds_jarvis()));
        for event in self.tools.drama3.events() {
            // A sentence Jarvis could not say after every retry is spoken by the lighter voice; that switch is
            // never silent ("it wasn't the proper Jarvis voice").
            if event["event"] == "voice_switch" {
                let label = event["notice"].as_str().unwrap_or(speak::drama3::NOTICE_MISSED);
                self.health_cue(Some(ui::HealthCue::Notice { part: ui::Part::Speech, label: label.into() }));
            }
            crate::diagnostics::record(&self.evidence,"drama3","heimdall:voice",event);
        }
        let drama_note=self.tools.drama3.status();
        let cue=if !drama_wanted || self.tools.drama3.policy_waiting() {self.health.clear("drama3")}
            else if let Some(note)=&drama_note {self.health.dropped("drama3",ui::Part::Speech,note)}
            else {self.health.recovered("drama3",ui::Part::Speech,"Drama 3 ready")};
        self.health_cue(cue);
        self.component_status("drama3",drama_note);
        let speech = self.tools.speech_status();
        let cue = match &speech {
            Some(_) => self.health.dropped("speech", ui::Part::Speech, "Speech unavailable"),
            None => self.health.recovered("speech", ui::Part::Speech, "Speech OK"),
        };
        self.health_cue(cue);
        self.component_status("speech", speech);
        self.component_status("agent-screen", self.tools.screenshot_status());
        self.component_status(
            "compaction",
            self._compactor.as_ref().and_then(|c| c.status()),
        );
        self.component_status("embedding-index",self._embeddings.as_ref().and_then(|j|j.status()));
        let status = self.disk_status.lock().unwrap().clone();
        if status != self.last_disk_status {
            if let Some(s) = &status {
                self.note_line(s);
            }
            self.component_status_reason("archive", status.as_ref().map(|_| "Archive paused".into()), status.as_deref());
            // "archive pipeline stopped" restarts capture (archive_stopped); the disk check's own words show here.
            if !status.as_deref().is_some_and(|s| s.starts_with("archive pipeline stopped")) {
                self.disk_health(status.as_deref());
            }
            self.last_disk_status = status;
            self.runtime_settings_ui();
        }
    }
    pub fn gap(&mut self, id: &str, ns: u64, why: &str) {
        if let Some(night) = &self.night { night.activity().speech(id, false); }
        self.drops.forget(id);
        self.component_status(&format!("gap:{id}"), Some("Audio gap recorded".into()));
        let later_active = self.active.get(id).is_some_and(|u| u.start >= ns);
        if !later_active {
            self.tools.voice(id, false);
            self.tools.speech_policy.other_voice(id, None);
            self.speaker_names.retain(|t, _| t.source != id);
            self.quiet_since.remove(id);
        }
        self.gaps += 1;
        if let Err(e) = self
            .index
            .note("gap", id, ns, &serde_json::json!({"reason":why}))
        {
            self.errors += 1;
            eprintln!("Gap archive failed: {e}");
        }
        self.source_cutoffs.entry(id.into()).and_modify(|cutoff| *cutoff = (*cutoff).max(ns)).or_insert(ns);
        if !later_active { if let Some(e) = self.engine.as_mut() { e.discard_source(id); } }
        // A worker gap is delivered after later capture PCM may already be in this ring.
        // Drop only the invalid prefix, never valid PCM after the loss boundary.
        if let Some(ring) = self.audio.get_mut(id) { ring.discard_before(ns); }
        if !later_active {
            self.correction_end(id,0);
            let _=self.correction_tick();
            self.discord.remove(id);
            self.reset_commands(id);
            self.archive_waiting_calls(id,"capture_gap_before_diarization");
            if let Some(u) = self.active.remove(id) {
                let mut segments = vec![ui::Segment::Words(
                    listen::Transcript::from_words(u.prefix).text,
                )];
                segments.push(ui::Segment::Note(format!("incomplete: {why}")));
                self.ui(ui::UiUpdate::Transcript(ui::TranscriptLine {
                    id: u.id,
                    label: None,
                    segments,
                    is_final: true,
                }));
            }
        }
        self.surface_gap_notice(id, why);
    }
    pub(super) fn surface_gap_notice(&mut self, id: &str, why: &str) {
        // The archive keeps every gap with its exact reason. The transcript shows only a failure the user should act
        // on, once per source and kind, in words: "MacBook Pro Microphone disconnected · 14:02".
        if let Some(what) = gap_notice(why) {
            let name = self.sources.get(id).map_or_else(|| display_name_from_id(id), display_name);
            if self.gap_notices.insert((name.clone(), what)) {
                self.note_line(&format!("{name} {what} · {}", crate::bridge::clock_label()));
            }
        }
    }
    pub fn finish_source(&mut self, id: &str) -> Result<()> {
        if let Some(worker) = &self.live_worker {
            worker.flush_source(id)?;
            return Ok(());
        }
        if let Some(e) = self.engine.as_mut() {
            let events = e.remove_source(id)?;
            for event in events {
                if let Err(e) = self.listen(event) {
                    self.errors += 1;
                    eprintln!("Final processing failed; continuing source teardown: {e:#}");
                }
            }
        }
        if let Some(d) = self.discord.get_mut(id) {
            // SourceClosed already queued the actor's final snapshot. Sending Finish twice
            // races its receiver's exit; drain that snapshot instead of restarting teardown.
            if !d.closed {
                d.actor.send(crate::speakers_runtime::Work::Finish)?;
                d.closed = true;
            }
            while self.discord.contains_key(id) {
                let event = self.speaker_events_rx.recv()?;
                self.speaker_update(event)?;
            }
        } else {
            self.close_source(id);
        }
        Ok(())
    }
}
/// A device back within this long after its removal is a route change, logged as its return.
const READDED_MS: u64 = 5_000;
/// A source id without its capture generation (`mic:<uid>:<generation>`, `process:<pid>:<birth>:<generation>`): the
/// same device across a re-add. An id of one kind and one name (`screen:1`) has no generation to strip: `screen:1` and
/// `screen:2` are two devices (review 27 Oct).
pub(super) fn device_key(id: &str) -> &str {
    match id.rsplit_once(':') {
        Some((device, generation)) if device.contains(':') && !generation.is_empty() && generation.chars().all(|c| c.is_ascii_digit()) => device,
        _ => id,
    }
}

/// Whether Jarvis should be resident: Speaking on (or residency "always") with Jarvis the chosen voice. Listening,
/// Seeing and pausing have no say (bug note 2026-10-30T18-42-08: "Speaking on must load him regardless of Listening").
/// `settings.paused` is Listening off (it never enters here: 1 Nov, gating on it kept Jarvis out exactly in the user's
/// setup). With the Agent off (`dry_run`) nothing speaks, so Speaking alone does not keep 27 GB resident.
pub(crate) fn jarvis_wanted(speak_replies: bool, dry_run: bool, residency: &str, route: crate::speech_voice::Route) -> bool {
    ((speak_replies && !dry_run) || residency == "always") &&
        // Speaking ▸ the voice: Jarvis (29 GB) stays out of memory while another voice is chosen (night 1022 menu).
        route == crate::speech_voice::Route::Jarvis
}
#[cfg(test)]
mod jarvis_residency_tests {
    use super::jarvis_wanted;
    use crate::speech_voice::Route;

    /// Bug note 2026-10-30T18-42-08: the residency decision takes no Listening input, and the live loop's settings
    /// tick (not only a capture event) applies it.
    #[test]
    fn speaking_on_with_jarvis_chosen_wants_him_whatever_listening_says() {
        assert!(jarvis_wanted(true, false, "speaking", Route::Jarvis));
        assert!(jarvis_wanted(false, false, "always", Route::Jarvis));
        assert!(!jarvis_wanted(false, false, "speaking", Route::Jarvis));
        assert!(!jarvis_wanted(true, false, "speaking", Route::Supertonic));
        assert!(!jarvis_wanted(true, true, "speaking", Route::Jarvis), "Agent off: nothing speaks");
        // The user's 1 Nov settings (Listening off is `paused: true`): the tick's call takes no paused input at all.
        let src = include_str!("sources.rs");
        let call = &src[src.find("let drama_wanted=").unwrap()..];
        let call = &call[..call.find(';').unwrap()];
        assert!(!call.contains("paused"), "Listening off (paused) must not keep Jarvis out: {call}");
        let src = include_str!("../live_loop.rs");
        let tick = src.find("recv(settings_tick)").expect("the settings tick");
        let body = &src[tick..tick + 1500];
        assert!(body.contains("app.component_tick()"), "the settings tick runs the residency check");
    }
}
