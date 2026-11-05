mod barge_in;
pub(crate) mod heimdall_voice;
mod mic_voice;
mod resilience;
mod ack;
mod send_worker;
/// Decider ▸ precision: the status the menu shows and the hot-swap (owner-thread half in `decider_swap`).
mod decider;
mod decider_swap;
pub(crate) mod speech_split;
pub use resilience::Internal;
pub(crate) use resilience::archive_damaged;

mod commands;
mod sources;
#[cfg(test)]
pub(crate) use sources::jarvis_wanted;
mod utterances;
mod speaker_tracks;
mod settings_apply;
mod menu_problem;
mod actions;
pub(crate) use actions::app_owns_card;
pub(crate) mod engine_models;
/// [chrome] n1022 stage 3: naming "Speaker N" from the transcript window.
mod transcript_naming;
mod voice_names;
#[cfg(test)]
pub(crate) use voice_names::latest_unnamed as latest_unnamed_for_tests;
/// `name_voice`'s `voice` words for the unnamed voice that spoke last (voice_names.rs).
pub(crate) const LATEST_VOICE: &[&str] = &["latest", "last", "most recent"];
mod suggest;
mod hotkeys;
mod listen_to;
// Night pass 3 (delivery): one utterance heard on two microphones is one utterance.
mod heard_twice;
// Drops onto the notch: held for the user's words or sent alone (24 Oct).
pub(crate) mod drops;
pub(crate) mod output_route;
pub(crate) mod echo_guard;
// Dictating to Vella: archived and given as context, never a wake (docs/vella.md).
mod vella;
mod catch_up;
mod model_downloads;
pub(crate) mod setup_ready;
use self::commands::*;

/// The spoken commands, matched in partials, window decodes and finals: Decider V5's two screen commands, each
/// confirmed with the question it was trained on (`decide::CommandSelection::check_question`). 
/// the decider only confirms "look at this" and "take a screenshot". Cancel-task and the "stop completely" family
/// were removed (20:55: "cancel the task" said while discussing commands cancelled the agent's task); other screen
/// aliases (`screen::SCREEN_PHRASES`) reach the agent as ordinary speech.
pub(crate) const COMMAND_PHRASES: &[&str] = &["look at this", "take a screenshot"];
use crate::{
    archive::{self, RawArchive},
    bridge::{AgentRuntime, Command, Tools},
    index::Index,
    settings::{self, Settings},
};
use anyhow::{ensure, Context, Result};
use listen::{ListenEngine, ListenEvent, Word};
use speakers::{
    identity::EnrollmentStore,
    naming::{NamingFlow, SpeakerEvent, TrackId},
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{atomic::Ordering, Arc, Mutex},
    time::Instant,
};


struct Discord {
    origin: u64,
    session: u64,
    actor_generation: u64,
    actor: crate::speakers_runtime::Actor,
    speaking: bool,
    fed_until: u64,
    active: bool,
    loading: bool,
    closed: bool,
    segments: Vec<speakers::diar::SpeakerSegment>,
    unlabelled: Vec<(u64, u64, u64, String, String)>,
}
#[derive(Default)]
struct Utterance {
    id: u64,
    start: u64,
    prefix: Vec<Word>,
    notes: Vec<String>,
    images: Vec<agent::protocol::Attachment>,
}
struct AgentFinal {
    audio_error:Option<String>,
    observation: agent::protocol::Observation,
    source: String,
    start: u64,
    end: u64,
    words: Vec<Word>,
    notes: Vec<String>,
    images: Vec<agent::protocol::Attachment>,
    turn: decide::Turn,
    /// Exact, possibly disjoint PCM ranges when only part of a turn goes to the agent. Always empty since spoken
    /// bug notes (the only thing that split a turn) were removed; kept so send_worker's record shape is unchanged.
    audio_spans: Vec<serde_json::Value>,
    forced: bool,
    end_of_turn_probability: f32,
}
#[derive(Clone)]
struct AudioRing {
    start: u64,
    samples: VecDeque<f32>,
}
impl AudioRing {
    fn end(&self) -> u64 {
        self.start + self.samples.len() as u64 * 62500
    }
    fn push(&mut self, ns: u64, a: &[f32]) {
        let expected = self.start + self.samples.len() as u64 * 62500;
        if expected.abs_diff(ns) > 125000 {
            self.samples.clear();
            self.start = ns;
        } else if ns >= expected {
            self.start = self.start.saturating_add(ns - expected);
        } else {
            self.start = self.start.saturating_sub(expected - ns);
        }
        self.samples.extend(a);
        let n = self.samples.len().saturating_sub(480000);
        self.samples.drain(..n);
        self.start += n as u64 * 62500;
    }
    fn discard_before(&mut self, boundary: u64) {
        let count = boundary.saturating_sub(self.start).div_ceil(62500).min(self.samples.len() as u64) as usize;
        self.samples.drain(..count);
        self.start += count as u64 * 62500;
    }
    fn clip(&self, start: u64, end: u64) -> Result<Vec<f32>> {
        let a = (start.saturating_sub(self.start) / 62500) as usize;
        let b = (end.saturating_sub(self.start) / 62500) as usize;
        let out: Vec<_> = self
            .samples
            .iter()
            .skip(a)
            .take(b.min(self.samples.len()).saturating_sub(a))
            .map(|v| v.clamp(-1., 1.))
            .collect();
        ensure!(
            !out.is_empty(),
            "utterance waveform unavailable after capture gap"
        );
        Ok(out)
    }
}
// Prior audio is never serialized into d1 state; avoid retaining it in jobs.
fn text_turn(turn:&decide::Turn)->decide::Turn {decide::Turn {id:turn.id.clone(),role:turn.role.clone(),transcript:turn.transcript.clone(),audio:decide::AudioClip::default()}}
const COMMAND_MERGE_NS: u64 = 1_000_000_000;
#[derive(Clone, serde::Serialize)]
struct Proposal {
    origin: &'static str,
    candidate: decide::CommandCandidate,
}
#[derive(Clone)]
struct PendingCommand {
    candidate: decide::CommandCandidate,
    proposals: Vec<Proposal>,
}
/// The decider reads the last <=15 s (Parakeet's window) of the turn, up to 200 ms after the trigger.
const SIDE_MAX_NS: u64 = 15_000_000_000;
const SIDE_AFTER_NS: u64 = 200_000_000;
/// Where a phrase is assumed to begin when its words are not in the text.
const PHRASE_FALLBACK_NS: u64 = 1_000_000_000;
impl std::ops::Deref for PendingCommand {
    type Target = decide::CommandCandidate;
    fn deref(&self) -> &Self::Target { &self.candidate }
}
struct CompletedCommand {
    source: String,
    utterance_id: String,
    proposals: Vec<Proposal>,
    approved: Option<decide::CommandSelection>,
}
pub struct Pipeline {
    pub settings: Settings,
    listen_settings: Arc<Mutex<Settings>>,
    pub pending_reload: BTreeMap<String, String>,
    // (dry_run, paused) last written to or accepted from settings.json.
    persisted_stop: (bool, bool),
    // The last settings write failed; the settings tick retries it. Noted once per failure run.
    save_failed: bool,
    save_failure_noted: bool,
    pub data: PathBuf,
    pub session: PathBuf,
    pub models: PathBuf,
    pub ui: Option<ui::Ui>,
    pub disk_status: Arc<Mutex<Option<String>>>,
    pub enabled: Arc<Mutex<HashMap<String, bool>>>,
    pub tools: Arc<Tools>,
    inference_engine: Option<crate::inference_engine::Manager>,
    /// The engine's models on this Mac: disk state, the download, what the manager runs (pipeline/engine_models.rs).
    engine_models: engine_models::EngineModels,
    /// The first-run downloads as last seen by the settings tick (model_downloads.rs): (listening's, the chosen decider's).
    models_seen: (bool, bool),
    /// Setup's agent check (setup_ready.rs), and the readiness rows last sent.
    pub(crate) agent_check: setup_ready::AgentCheck,
    readiness_seen: Option<ui::SetupReadiness>,
    status_written: Option<Instant>,
    /// `Heimdall setup --arsenal add` installed Arsenal: it is used once it is here.
    arsenal_after_install: bool,
    /// `Heimdall setup` chose Claude Code while T3 Code installed: it connects once T3 Code is here.
    t3_connect_after_install: bool,
    /// Setup loaded (or tried to load) Phonon S to learn the chosen voice this launch (setup_ready.rs).
    pub(crate) phonon_tried: bool,
    engine: Option<ListenEngine<listen::asr::Parakeet>>,
    live_worker: Option<Arc<listen::ListenWorker>>,
    live_stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Listening was turned on at this time and the island waits for "Listening" (models loaded).
    listening_since: Option<Instant>,
    /// Unexpected listen-worker ends, for the restart bound.
    worker_restarts: VecDeque<Instant>,
    send_worker: Option<send_worker::InferenceOwner>,
    /// Decider ▸ a precision swap in flight (its target), what the decider does, and the last failure's reason.
    /// (from, to) while a swap runs.
    decider_swapping: Option<(decide::precision::Pick, decide::precision::Pick)>,
    decider_phase: decider::Phase,
    decider_reason: Option<String>,
    agent: Option<AgentRuntime>,
    sources: HashMap<String, capture::SourceInfo>,
    effective_primary_uid: Option<String>,
    /// What the Microphone and App Audio submenus list now: the `settings` tool finds devices and apps by these names.
    menu_lists: (ui::Microphones, ui::AppAudio),
    primary_reported: bool,
    active: HashMap<String, Utterance>,
    audio: HashMap<String, AudioRing>,
    context: VecDeque<decide::Turn>,
    command_screen: screen::PhraseMatcher,
    /// The command table over window-decode segments: each occurrence once per utterance.
    command_window: screen::PhraseMatcher,
    command_candidates: VecDeque<PendingCommand>,
    command_inflight: HashMap<String,usize>,
    command_deferred: HashMap<String,VecDeque<ListenEvent>>,
    early_ids: HashMap<(String,u64),u64>,
    dispatched_through:u64,
    early_sends: HashMap<String,(Option<agent::delivery::DeliveryPermit>,u64)>,
    completed_commands: VecDeque<CompletedCommand>,
    /// Utterances (observation and turn ids) that named a fixed stop phrase in an explicit address: the harness
    /// stopped, and no wake may carry them to the model (E3.7).
    /// Microphone finals kept to recognise the same speech heard on a second microphone (`heard_twice`).
    heard_twice: heard_twice::HeardTwice,
    /// Screen pixels pinned when a screenshot phrase is detected, until its decision.
    pub(crate) screen_holds: screen::ScreenHolds,
    /// The user talking while Heimdall speaks, until the decider judges it (pipeline/barge_in.rs).
    barge: Option<barge_in::BargeIn>,
    /// Audio time before which ongoing speech cannot arm a barge-in: a stopped reply takes a moment to
    /// leave the speech queue, and the speech that stopped it must not count again.
    barge_rest_until: u64,
    /// Heimdall's own voices and the encoder that checks the microphone against them (heimdall_voice.rs).
    heimdall_voice: heimdall_voice::State,
    /// Who speaks on his microphone, per utterance and per admitted line (mic_voice.rs).
    mic_voices: mic_voice::MicVoices,
    /// When the own-voice encoder was last asked to start ahead of a check.
    voice_warmed: Option<Instant>,
    night: Option<crate::night_runtime::Scheduler>,
    speech_split: speech_split::State,
    awaiting_speaker: HashMap<u64, AgentFinal>,
    #[cfg(test)]
    command_test_answers: VecDeque<(decide::CommandIntent, decide::CommandSelection)>,
    #[cfg(test)]
    hooks: plumbing_tests::Hooks,
    pub screen_buffer: Arc<Mutex<screen::ScreenBuffer>>,
    /// Capture still starting on its own thread (live_loop.rs `CaptureRun::begin`): HAL discovery may wait up to 10 s
    /// (30 s in a VM without audio devices, VM run 5 Nov), which on the live loop tripped the watchdog.
    pub(crate) capture_starting: Option<crossbeam_channel::Receiver<Result<(capture::Capture, capture::CaptureReceiver)>>>,
    /// A Seeing on still starting on its own thread (live_loop.rs `start_screen`), with the config it started with.
    pub(crate) screen_starting: Option<(crossbeam_channel::Receiver<Result<screen::ScreenCapture>>, String, Arc<std::sync::atomic::AtomicBool>)>,
    /// The newest screen start or stop running off the live loop. Each joins the one before, so stops and starts
    /// reach the archive in click order (a late stop's display gap would cut a newer stream's segment).
    pub(crate) screen_lane: Option<std::thread::JoinHandle<()>>,
    pub speaker_cpu: Arc<crate::metrics::SpeakerCpu>,
    enrollment_lock: Arc<Mutex<()>>,
    /// Named guests Heimdall listens to in calls, as `speakers.json` has them (listen_to.rs).
    listened: Mutex<std::collections::BTreeSet<String>>,
    /// `speakers.json` as of the last Speakers list sent (speaker_tracks.rs): a diarization frame with no events skips
    /// the parse and the send while the file is unchanged. Outer `None`: nothing sent yet.
    speakers_sent: Mutex<Option<Option<speaker_tracks::StoreStamp>>>,
    /// The user's microphone lines not yet carried by a wake, by conversation sequence (listen_to.rs).
    owner_words: VecDeque<(u64, u64, String)>,
    speaker_tx: crossbeam_channel::Sender<crate::speakers_runtime::Event>,
    pub speaker_events_rx: crossbeam_channel::Receiver<crate::speakers_runtime::Event>,
    discord: HashMap<String, Discord>,
    pub index: Index,
    evidence: heimdall_archive::records::Writer,
    _compactor: Option<heimdall_archive::compact::Compactor>,
    _embeddings: Option<crate::embeddings::Jobs>,
    next_id: u64,
    next_side_id: u64,
    labels: HashMap<u64, (ui::TranscriptLine, Option<TrackId>)>,
    pub finals: usize,
    pub decisions: usize,
    pub notes: usize,
    pub errors: usize,
    pub gaps: usize,
    pub segments: usize,
    /// Voice naming cards shown (the old per-voice popup is gone:).
    pub speaker_prompts: usize,
    /// New voices given a code label.
    pub coded_voices: usize,
    voice_namer: voice_names::VoiceNamer,
    suggest: suggest::Suggest,
    /// ⌃⌘\ and ⌃⌘': the user's live labels for the decider (hotkeys.rs).
    hotkeys: hotkeys::Hotkeys,
    replay: bool,
    attachment_batch: Vec<agent::protocol::Attachment>,
    /// Drops onto the notch waiting for the user's words (`drops.rs`).
    drops: drops::DropHold,
    /// Vella's dictation state and keybind (`crate::vella`, pipeline/vella.rs).
    pub(crate) vella: crate::vella::Link,
    /// Per pending attachment, in batch order: the conversation it belongs to (`through`) and its capture time.
    attachment_stamps: Vec<(u64, u64)>,
    pending_text: VecDeque<String>,
    last_disk_status: Option<String>,
    component_status: BTreeMap<String, String>,
    /// What the menu shows for each held status (`menu_problem`): only problems the user can act on.
    menu_problems: BTreeMap<String, String>,
    component_causes: BTreeMap<String, String>,
    last_screen_event: Option<String>,
    /// When the island's "catching up" words last changed (`catch_up.rs`).
    catch_up_shown: Option<std::time::Instant>,
    /// The archive's writer fell behind since it last caught up (`catch_up.rs`): its drain restarts a screen that gave up.
    catch_up_backlog: bool,
    /// Writes the archive had dropped when the island last cleared (`catch_up.rs`): more since then show at once.
    catch_up_dropped: Option<usize>,
    /// The screen stream spent its restarts and waits for the OS: the archive catching up restarts it too
    /// (`catch_up.rs`, bug note 2026-11-02T15-58-15).
    pub(crate) screen_gave_up: bool,
    /// The last failure, when it was first noted and how many repeats since were only counted (`fail`).
    repeated_failure: Option<(String, std::time::Instant, usize)>,
    failure: Option<String>,
    speaker_names: BTreeMap<TrackId, String>,
    source_cutoffs: HashMap<String, u64>,
    /// When another call participant's line last ended (host ns): a call is going on (night review D3).
    last_call_line_ns: Option<u64>,
    /// When a goodbye in the call last ended (his or theirs): the call is over (`decide::send::call_active`).
    call_farewell_ns: Option<u64>,
    quiet_since: HashMap<String, u64>,
    /// Decider v3: a wake sent before d1 answered, whose ack d1's verdict on the same words may still pick (ack.rs).
    late_ack: Option<ack::LateAck>,
    /// The island's send arrow and thinking dots wait for the ack's moment on this source (host ns; `u64::MAX`
    /// while the user talks): ack.rs `hold_island`.
    island_hold: Option<(String, u64)>,
    gap_notices: std::collections::HashSet<(String, &'static str)>,
    /// When each device last left capture, by its id without the generation (`pipeline/sources.rs`).
    removed_sources: std::collections::HashMap<String, u64>,
    pub bugs: crate::bugs::BugNotes,
    /// Health on the island and the recovery ladder (pipeline/resilience.rs, docs/resilience.md).
    pub(crate) health: crate::health::Health,
    /// Listening is on (capture asked for); health about live parts only applies then.
    pub(crate) listening: bool,
    /// Each open microphone source and when its audio last reached this loop: the capture-stall check.
    mic_audio: HashMap<String, Instant>,
    /// When capture last started (Setup's Microphone row waits `MIC_SILENT` for sound from then).
    pub(crate) capture_since: Option<Instant>,
    /// System Audio Recording read as denied from its silent taps (`live_loop::tap_playing`): Getting ready's grey row.
    pub(crate) system_audio_denied: bool,
    capture_ladder: crate::health::Ladder,
    agent_ladder: crate::health::Ladder,
    /// The listen worker's finished-work count and since when it stood still with work waiting.
    worker_watch: Option<(u64, Instant)>,
    worker_spawned: Option<Instant>,
    recovery: Option<crate::health::Recovery>,
    mics: crate::health::MicWatch,
    /// Set at launch after a relaunch: the island says "Heimdall OK" once Heimdall runs again.
    pub(crate) relaunched: Option<String>,
    /// Agent pre-flight: the newest check's number (older answers are ignored) and the channel answers arrive on.
    preflight: u64,
    agent_preflight_pending: Option<u64>,
    internal: (crossbeam_channel::Sender<Internal>, crossbeam_channel::Receiver<Internal>),
}
fn is_discord_bundle(source: &capture::SourceInfo) -> bool {
    match &source.kind {
        capture::SourceKind::Process {
            bundle_id, owner, ..
        } => owner
            .as_ref()
            .and_then(|o| o.bundle_id.as_deref())
            .or(bundle_id.as_deref())
            .is_some_and(|b| {
                matches!(
                    b,
                    "com.hnc.Discord" | "com.hnc.DiscordPTB" | "com.hnc.DiscordCanary"
                )
            }),
        _ => false,
    }
}

/// What the transcript calls a source: the device or app name, never ids, bundle ids or PIDs.
pub fn display_name(i: &capture::SourceInfo) -> String {
    match &i.kind {
        capture::SourceKind::InputDevice { name, .. } => name.clone(),
        capture::SourceKind::Process { name, owner, .. } => owner.as_ref().map_or_else(|| name.clone(), |o| o.name.clone()),
    }
}
fn agent_source_name(i: &capture::SourceInfo) -> String {
    match &i.kind {
        capture::SourceKind::InputDevice { .. } => "Mic".into(),
        _ => display_name(i),
    }
}
fn display_name_from_id(id: &str) -> String {
    if id.starts_with("mic:") { "Microphone".into() } else { "App audio".into() }
}
/// A gap reason worth a transcript line, in words; `None` keeps it archive-only (overflow, discontinuity,
/// unverified digital silence, deliberate pauses).
fn gap_notice(why: &str) -> Option<&'static str> {
    let w = why.to_lowercase();
    if w.contains("unverified") {
        None
    } else if w.contains("permission") && (w.contains("denied") || w.contains("revoked")) {
        Some("lost its permission")
    } else if w.contains("disconnected") || w.contains("device removed") || w.contains("device lost") {
        Some("disconnected")
    } else if w.starts_with("archive pipeline stopped") {
        Some("stopped archiving")
    } else {
        None
    }
}
impl Pipeline {
    pub fn new(
        data: PathBuf,
        models: PathBuf,
        settings: Settings,
        ui: Option<ui::Ui>,
        replay: bool,
    ) -> Result<Self> {
        let tools = Arc::new(Tools::new(&models, &data));
        tools.initialize_delivery(&settings);
        Self::new_with_tools(data, models, settings, ui, replay, tools)
    }
    pub fn new_with_tools(
        data: PathBuf,
        models: PathBuf,
        mut settings: Settings,
        ui: Option<ui::Ui>,
        replay: bool,
        tools: Arc<Tools>,
    ) -> Result<Self> {
        tools.restore_send_context()?;
        tools.api.set_ui(ui);
        if ui.is_some() { crate::bridge::attach_speech_level(ui, Arc::downgrade(&tools)); }
        tools.api.set_settings(&settings);
        {
            // A connected, replaced or forgotten key restarts an agent running on that provider.
            let weak = Arc::downgrade(&tools);
            tools.api.on_key_changed(move |provider| {
                let Some(tools) = weak.upgrade() else { return };
                let tx = tools.agent_commands.lock().unwrap().clone();
                if let Some(tx) = tx { let _ = tx.send(Command::ProviderKeyChanged(provider.into())); }
            });
        }
        let persisted_stop = (settings.dry_run, settings.paused);
        // Startup settings may lag a callback that stopped the previous frozen process.
        settings.dry_run = tools.agent_enabled.stopped_for(agent::delivery::StopReason::Agent);
        settings.paused = tools.stopped_for(agent::delivery::StopReason::Heimdall);
        settings.normalize_archive();
        settings::private_dir(&data)?;
        let data_for_bugs = data.clone();
        let models_for_night = models.clone();
        let root = data.join("archive");
        settings::private_dir(&root)?;
        archive::prune(&root, settings.retention_days, None)?;
        let session = archive::session_dir(&root)?;
        // A crashed launch's working files go into its session's logs before this one starts.
        if let Err(e) = archive::retire_stale_runs(&data, &root, &session) {
            eprintln!("earlier launch's run files kept: {e:#}");
        }
        let disk_status = Arc::new(Mutex::new(None));
        let mut index = Index::open(
            &session,
            settings.free_space_floor_bytes,
            disk_status.clone(),
        )?;
        index.evidence.event("timeline_settings", "heimdall", &[], capture::host_now_ns(), serde_json::json!({"settings": settings}))?;
        let evidence = heimdall_archive::records::Writer::open(&session)?;
        let compactor = if replay {
            None
        } else {
            // On battery the cold tier waits unless the disk nears the floor.
            let gate = archive::compaction_gate(root.clone(), tools.archive_floor.clone());
            Some(heimdall_archive::compact::Compactor::start_gated(root.clone(), gate)?)
        };
        let embeddings=if replay {None}else{Some(crate::embeddings::Jobs::start(root,tools.embeddings.clone(),tools.archive_floor.clone())?)};
        let screen_buffer = Arc::new(Mutex::new(screen::ScreenBuffer::new(screen::ScreenConfig { fps: if replay {1}else{5}, ..Default::default() })?));
        *tools.archive_session.lock().unwrap() = Some(session.clone());
        tools.persist_send_context(&tools.send_context.lock().unwrap())?;
        tools
            .archive_floor
            .store(settings.free_space_floor_bytes, Ordering::Release);
        tools
            .enabled
            .store(settings.speak_replies, Ordering::Release);
        *tools.tts_backend.lock().unwrap() = settings.tts_backend.clone();
        tools.screen_off.store(settings.screen_off, Ordering::Release);
        tools.voice_speed.store(settings.voice_speed_percent, Ordering::Release);
        crate::background_model::publish(&settings.background_model, &settings.background_off); // read by every background job from launch
        crate::background_model::publish_efforts(&settings.background_efforts);
        crate::background_model::publish_tasks(&settings.task_models);
        crate::background_model::publish_engine(settings.engine_on);
        crate::background_model::publish_fallback_paid(settings.background_fallback_paid);
        crate::islands::streams::set(settings.islands_on_streams); // Show Islands on Streams (main sets it before the helper starts)
        if !replay {
            // Every background job's run counts in the spend ledger (they all run on Anthropic: `background_model`).
            let hub = tools.api.clone();
            crate::background_call::set_ledger(Arc::new(move |run| hub.record_background(agent::provider::ANTHROPIC, run)));
            // A background task off its pick (a spent quota, a backend down): Background Tasks' small sign.
            let routes = Arc::downgrade(&tools);
            crate::background_fallback::set_routes_sink(Arc::new(move |r| if let Some(t) = routes.upgrade() { t.emergency_ui(ui::UiUpdate::BackgroundRoutes(r)) }));
            // The Claude and Codex accounts' quota, read only when a fallback's or the relay's choice depends on it, at most
            // every 20 minutes for the fallback, 5 for the relay (personal edition: the Vault's `Tools/subscription_quota.py`).
            #[cfg(not(test))]
            if !crate::edition::PUBLIC {
                if let (Some(user), home) = (std::env::var_os("HOME"), heimdall_home::home_dir()) {
                    let user = std::path::Path::new(&user);
                    if let Some(mut script) = agent::accounts::Script::in_home(&home, user) {
                        // T3's own accounts: the relay picks among them from the same reading (`emergency::T3Cover::quotas`).
                        script.t3_settings = Some(user.join(".t3/userdata/settings.json"));
                        crate::background_fallback::set_quota_board(agent::accounts::Board::new(Arc::new(script), agent::accounts::FALLBACK_TTL));
                    }
                }
            }
            // With Agent off no background job asks a model (`background_gate`).
            crate::background_gate::set(tools.agent_enabled.clone());
            // Nothing can cover: the local engine's offer reads the engine and settings from here.
            crate::emergency_local::attach(&data);
        }
        tools.speech_voices.set_choice(settings.speech_voice.clone()); // Speaking ▸ the voice (night 1022 menu)
        tools.enabled.store(settings.speak_replies, Ordering::Release);
        // speech-policy (night 3): Speaking ▸ Wake Sound; a replay never sounds on the speakers.
        tools.speech_policy.set_cue_enabled(settings.wake_sound);
        tools.speech_policy.set_speaking(settings.speak_replies);
        tools.speech_policy.set_echo_cancel_off(settings.echo_cancel_off);
        if replay { tools.speech_policy.set_player(Arc::new(|| {})); }
        // The callback may already have revoked delivery during initialization.
        index.set_floor_source(tools.archive_floor.clone());
        // During-call correction owns overlap work. Do not run the old idle worker
        // alongside it: that would load a second Parakeet and duplicate corrections.
        let night = None;
        let _ = models_for_night;

        if !replay { crate::voice_bank::start_fill(&tools); }
        let (speaker_tx, speaker_events_rx) = crossbeam_channel::bounded(16);
        // Bug notes: Heimdall's `bug_note` tool writes to this inbox; the menu lists it (bugs.rs).
        let bug_inbox = crate::bugs::inbox_dir(&data_for_bugs,replay);
        // The public edition has no bug notes (docs/bugs.md): no inbox for the tool and no scout.
        #[cfg(not(feature = "public"))]
        {
            crate::bugs::retire_spoken_note_checkpoints(&data_for_bugs);
            tools.mcp_actions.bug_notes.set_inbox(bug_inbox.clone());
            // A filed note starts its scout only in the installed app, on the project's inbox (never in replays or lab runs).
            tools.mcp_actions.bug_notes.set_scout(!replay && heimdall_night::bug_notes::default_inbox().is_some_and(|p| p == bug_inbox));
        }
        let internal = crossbeam_channel::unbounded();
        // The agent's `name_voice` reaches this pipeline through its internal queue (voice_names.rs).
        if !replay {
            *tools.mcp_actions.voice_names.lock().unwrap() = Some(internal.0.clone());
        }
        let listened = listen_to::listened_in(&data);
        let speaker_cpu = Arc::new(crate::metrics::SpeakerCpu::default());
        // Heimdall's own voices as enrolled so far (none in an older file), and the encoder for the check.
        let own_voices = EnrollmentStore::load(&data.join("speakers.json")).map(|s| s.heimdall).unwrap_or_default();
        let user_voice_path = data.join(heimdall_voice::USER_FILE);
        let voice_encoder = (!replay && !cfg!(test)).then(|| Arc::new(heimdall_voice::Encoder::start(models.join("speakers"), speaker_cpu.clone())));
        let engine_models = engine_models::EngineModels::new(&data);
        Ok(Self {
            listen_settings: Arc::new(Mutex::new(settings.clone())),
            pending_reload: BTreeMap::new(),
            persisted_stop,
            save_failed: false,
            save_failure_noted: false,
            settings,
            data,
            session: session.clone(),
            models,
            ui,
            disk_status,
            enabled: Arc::new(Mutex::new(HashMap::new())),
            tools,
            inference_engine: None,
            engine_models,
            models_seen: (false, false),
            agent_check: Default::default(),
            readiness_seen: None,
            status_written: None,
            arsenal_after_install: false,
            t3_connect_after_install: false,
            phonon_tried: false,
            engine: None,
            live_worker: None,
            live_stop: None,
            listening_since: None,
            worker_restarts: VecDeque::new(),
            send_worker: None,
            decider_swapping: None,
            decider_phase: Default::default(),
            decider_reason: None,
            agent: None,
            sources: HashMap::new(),
            effective_primary_uid: None,
            menu_lists: Default::default(),
            primary_reported: false,
            active: HashMap::new(),
            audio: HashMap::new(),
            context: VecDeque::new(),
            command_screen: screen::PhraseMatcher::new(COMMAND_PHRASES),
            command_window: screen::PhraseMatcher::new(COMMAND_PHRASES),
            command_candidates: VecDeque::new(),
            command_inflight:HashMap::new(), command_deferred:HashMap::new(), early_ids:HashMap::new(),dispatched_through:0,early_sends:HashMap::new(),
            completed_commands: VecDeque::new(),
            heard_twice: Default::default(),
            screen_holds: screen::ScreenHolds::new(if replay { None } else { screen::native_grabber() }),
            barge: None,
            barge_rest_until: 0,
            heimdall_voice: heimdall_voice::State::new(own_voices, voice_encoder, Some(user_voice_path)),
            mic_voices: Default::default(),
            voice_warmed: None,
            night,
            speech_split: speech_split::State::default(),
            awaiting_speaker: HashMap::new(),
            #[cfg(test)]
            command_test_answers: VecDeque::new(),
            #[cfg(test)]
            hooks: Default::default(),
            screen_buffer,
            screen_starting: None,
            capture_starting: None,
            screen_lane: None,
            speaker_cpu,
            enrollment_lock: Arc::new(Mutex::new(())),
            listened: Mutex::new(listened),
            speakers_sent: Mutex::new(None),
            owner_words: VecDeque::new(),
            speaker_tx,
            speaker_events_rx,
            discord: HashMap::new(),
            index,
            evidence,
            _compactor: compactor,
            _embeddings: embeddings,
            next_id: 1,
            next_side_id: 1,
            labels: HashMap::new(),
            finals: 0,
            decisions: 0,
            notes: 0,
            errors: 0,
            gaps: 0,
            segments: 0,
            speaker_prompts: 0,
            coded_voices: 0,
            voice_namer: Default::default(),
            suggest: Default::default(),
            hotkeys: Default::default(),
            replay,
            attachment_batch: vec![],
            drops: Default::default(),
            vella: Default::default(),
            attachment_stamps: vec![],
            pending_text: VecDeque::new(),
            last_disk_status: None,
            component_status: BTreeMap::new(),
            menu_problems: BTreeMap::new(),
            component_causes: BTreeMap::new(),
            last_screen_event: None,
            catch_up_shown: None,
            catch_up_backlog: false,
            catch_up_dropped: None,
            screen_gave_up: false,
            repeated_failure: None,
            failure: None,
            speaker_names: BTreeMap::new(),
            source_cutoffs: HashMap::new(),
            last_call_line_ns: None,
            call_farewell_ns: None,
            quiet_since: HashMap::new(),
            late_ack: None,
            island_hold: None,
            gap_notices: Default::default(),
            removed_sources: Default::default(),
            bugs: crate::bugs::BugNotes::new(bug_inbox),
            health: Default::default(),
            listening: false,
            mic_audio: HashMap::new(),
            capture_since: None,
            system_audio_denied: false,
            capture_ladder: crate::health::Ladder::new(std::time::Duration::from_secs(600)),
            agent_ladder: crate::health::Ladder::new(std::time::Duration::from_secs(600)),
            worker_watch: None,
            worker_spawned: None,
            recovery: None,
            mics: Default::default(),
            relaunched: None,
            preflight: 0,
            agent_preflight_pending: None,
            internal,
        })
    }
    pub(crate) fn component_status(&mut self, component: &str, message: Option<String>) {
        let reason = message.clone();
        self.component_status_reason(component, message, reason.as_deref());
    }
    pub(crate) fn component_status_reason(&mut self, component: &str, message: Option<String>, reason: Option<&str>) {
        if self.component_status.get(component) == message.as_ref()
            && self.component_causes.get(component).map(String::as_str) == reason {
            return;
        }
        crate::diagnostics::record(&self.evidence, "health", "heimdall", serde_json::json!({
            "component": component, "state": if message.is_some() { "unavailable" } else { "recovered" },
            "reason": reason, "previous": self.component_status.get(component),
        }));
        match reason {
            Some(reason) => { self.component_causes.insert(component.into(), reason.into()); },
            None => { self.component_causes.remove(component); },
        }
        if let Some(m) = &message {
            self.component_status.insert(component.into(), m.clone());
        } else {
            self.component_status.remove(component);
        }
        // The journal above keeps every status; the menu gets only what the user can act on, as a sentence.
        let shown = message.as_deref().and_then(|m| menu_problem::menu_problem(component, m, reason));
        if self.menu_problems.get(component) != shown.as_ref() {
            match &shown {
                Some(line) => { self.menu_problems.insert(component.into(), line.clone()); },
                None => { self.menu_problems.remove(component); },
            }
            self.ui(ui::UiUpdate::ComponentStatus { component: component.into(), message: shown });
        }
    }
    fn ui(&self, u: ui::UiUpdate) {
        crate::diagnostics::ui_record(&self.evidence, &u, self.ui.is_some());
        #[cfg(test)]
        self.hooks.ui.lock().unwrap().push(u.clone());
        if let Some(h) = self.ui {
            h.send(u);
        }
    }
    pub fn finish(&mut self) -> Result<()> {
        // Stop Splash before a potentially stalled archive/agent drain.
        if let Some(manager) = &mut self.inference_engine { manager.shutdown(); }
        self.inference_engine_events();
        self.inference_engine.take();
        if let Some(stop) = self.live_stop.take() {
            stop.store(true, Ordering::Release);
        }
        if let Some(worker) = self.live_worker.take() {
            worker.request_shutdown();
        }
        let ids: Vec<_> = self.sources.keys().cloned().collect();
        let mut failures = vec![];
        for id in ids {
            if let Err(e) = self.finish_source(&id) {
                failures.push(format!("{id}: {e:#}"));
                self.close_source(&id);
            }
            crate::watchdog::progress();
        }
        if let Some(a) = &self.agent {
            if let Err(e)=a.barrier() {failures.push(format!("agent drain: {e:#}"));}
        }
        crate::watchdog::progress();
        self.agent.take();
        self.send_worker.take();
        send_worker::join_retired();
        self.tools.shutdown();
        crate::watchdog::progress();
        if !failures.is_empty() {
            self.record_teardown_error(&format!("source teardown errors: {}", failures.join("; ")));
        }
        self.evidence.flush()?;
        crate::watchdog::progress();
        ensure!(failures.is_empty(), "source teardown errors: {}", failures.join("; "));
        heimdall_archive::store::mark_complete(&self.session)?;
        archive::retire_run(&archive::run_dir(&self.data, &self.session), &self.session)?;
        Ok(())
    }
}
impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Some(stop) = &self.live_stop {
            stop.store(true, Ordering::Release);
        }
        if let Some(worker) = &self.live_worker {
            worker.request_shutdown();
        }
        self.send_worker.take();
        send_worker::join_retired();
        self.tools.shutdown();
    }
}
fn install_listen_evidence(
    engine: &mut ListenEngine<listen::asr::Parakeet>,
    writer: heimdall_archive::records::Writer,
) {
    engine.set_evidence_hook(Box::new(move |source, ns, payload| {
        let ids = payload["utterance_start_ns"]
            .as_u64()
            .map(|start| vec![format!("listen:{source}:{start}")])
            .unwrap_or_default();
        writer.event("model_output", source, &ids, ns, payload)
    }));
}
/// The decider's training rows (`d1_question`), written in order on their own thread. Writing one waits for the
/// live loop's queued writes and reads every context turn's span from the shared index, which queues behind its
/// commits on a busy disk: at 15:45 on 26 Oct, under two builds, it took 6 s after a 72 ms model answer. Inside the
/// owner that time counted against the request's 3 s deadline, so the owner was declared hung three times and
/// Heimdall relaunched (bugs 2026-10-26T15-49-44, T15-52-01). A row that cannot be written is recorded, never a
/// failed decision.
#[derive(Clone)]
pub(crate) struct DecisionArchive {
    tx: crossbeam_channel::Sender<ArchiveRow>,
    writer: heimdall_archive::records::Writer,
}
pub(crate) type ArchiveRow = (decide::DecisionInput, Option<decide::Decision>, Option<String>, Option<serde_json::Value>);
impl DecisionArchive {
    pub(crate) fn start(session: PathBuf, writer: heimdall_archive::records::Writer) -> Result<Self> {
        let w = writer.clone();
        Self::start_with(writer, move |(input, decision, error, outputs)| {
            archive_decision(&session, &w, input, decision.as_ref(), error.as_deref(), outputs.as_ref())
        })
    }
    pub(crate) fn start_with(
        writer: heimdall_archive::records::Writer,
        mut write: impl FnMut(&ArchiveRow) -> Result<()> + Send + 'static,
    ) -> Result<Self> {
        let (tx, rx) = crossbeam_channel::bounded::<ArchiveRow>(64);
        let w = writer.clone();
        std::thread::Builder::new().name("heimdall-decision-archive".into()).spawn(move || {
            for row in rx {
                // One bad row must not end the thread: every later row would be lost with it.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write(&row))) {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => Self::lost(&w, &row.0, &format!("{e:#}")),
                    Err(_) => Self::lost(&w, &row.0, "writing the row panicked"),
                }
            }
        })?;
        Ok(Self { tx, writer })
    }
    pub(crate) fn queue(
        &self,
        input: &decide::DecisionInput,
        decision: Option<&decide::Decision>,
        error: Option<&str>,
        outputs: Option<&serde_json::Value>,
    ) {
        let row = (input.clone(), decision.cloned(), error.map(str::to_owned), outputs.cloned());
        match self.tx.try_send(row) {
            Ok(()) => {}
            Err(crossbeam_channel::TrySendError::Full(_)) => Self::lost(&self.writer, input, "the decision archive queue is full"),
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => Self::lost(&self.writer, input, "the decision archive thread stopped"),
        }
    }
    fn lost(writer: &heimdall_archive::records::Writer, input: &decide::DecisionInput, error: &str) {
        eprintln!("Decision row not archived: {error}");
        let _ = writer.event("decision_archive_failed", "heimdall", std::slice::from_ref(&input.current.id), capture::host_now_ns(),
            serde_json::json!({"error": error}));
    }
}
fn archive_decision(
    session: &std::path::Path,
    writer: &heimdall_archive::records::Writer,
    input: &decide::DecisionInput,
    decision: Option<&decide::Decision>,
    error: Option<&str>,
    model_outputs: Option<&serde_json::Value>,
) -> Result<()> {
    // The live loop's writes of these utterances may be queued behind a held archive lock (index.rs): wait a
    // little for them, and when they are still queued archive the decision without their spans rather than fail it.
    let settled = heimdall_archive::live::settle(&archive::store_root(session), std::time::Duration::from_secs(2));
    let db = heimdall_archive::store::read_index(&archive::store_root(session))?;
    let name = session.file_name().unwrap_or_default().to_string_lossy();
    let turn = |t: &decide::Turn| -> Result<serde_json::Value> {
        let bytes = heimdall_archive::audio::pcm_bytes(&t.audio.samples);
        // The read-only connection above: opening the index for writing per turn queued ~20 opens behind live commits.
        let span = db.query_row(
            "SELECT u.source_id,u.start_ns,u.end_ns FROM utterances u JOIN sessions s ON s.id=u.session WHERE s.name=?1 AND u.id=?2",
            rusqlite::params![name.as_ref(), t.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)));
        let (source, start, end) = match span {
            Ok(span) => span,
            Err(_) if !settled => return Ok(serde_json::json!({"id":t.id,"role":t.role,"transcript":t.transcript,
                "audio":{"utterance_id":t.id,"reference":"archive_write_pending","rate":t.audio.sample_rate,
                    "samples":t.audio.samples.len(),"pcm_sha256":heimdall_archive::hash(&bytes)}})),
            Err(e) => return Err(e.into()),
        };
        let disposition: String = db.query_row(
            "SELECT u.decision FROM utterances u JOIN sessions s ON s.id=u.session WHERE s.name=?1 AND u.id=?2",
            rusqlite::params![name.as_ref(),t.id], |r| r.get(0))?;
        let disposition: serde_json::Value = serde_json::from_str(&disposition)?;
        let mut audio = serde_json::json!({"utterance_id":t.id,"source_id":source,
            "start_ns":(end as u64).saturating_sub(t.audio.samples.len() as u64*62500).max(start as u64),
            "end_ns":end,"rate":t.audio.sample_rate,"samples":t.audio.samples.len(),
            "pcm_sha256":heimdall_archive::hash(&bytes),"transform":"capture mono/resample 16k; clip host interval; clamp [-1,1]"});
        if t.audio.samples.is_empty() {
            // Text-only jobs carry no PCM. Point training readers at the real
            // archived interval instead of inventing an empty clip/hash.
            audio=serde_json::json!({"utterance_id":t.id,"source_id":source,
                "start_ns":start,"end_ns":end,"reference":"archive_capture_span","model_input":"text_only"});
        }
        if let Some(spans) = disposition["agent_audio_spans"].as_array().filter(|s| !s.is_empty()) {
            audio["start_ns"] = spans.first().unwrap()["start_ns"].clone();
            audio["end_ns"] = spans.last().unwrap()["end_ns"].clone();
            audio["spans"] = serde_json::json!(spans);
            audio["transform"] = serde_json::json!("capture mono/resample 16k; concatenate listed half-open host spans in order; clamp [-1,1]");
        }
        Ok(serde_json::json!({"id":t.id,"role":t.role,"transcript":t.transcript,"audio":audio}))
    };
    let current = turn(&input.current)?;
    let context: Vec<_> = input.context.iter().map(turn).collect::<Result<_>>()?;
    let media = match &input.media {
        decide::DecisionMedia::Image(i) => {
            let journal = heimdall_archive::Journal::open(session)?;
            let id = journal.image(&i.bytes, &i.mime)?;
            serde_json::json!({"kind":"image","image_id":id})
        }
        other => serde_json::to_value(other)?,
    };
    let mut model_outputs = model_outputs.cloned();
    if let Some(value) = &mut model_outputs {
        heimdall_archive::Journal::open(session)?.reference_vectors(value)?;
    }
    let payload = serde_json::json!({"schema":2,"input":{"source":input.source,"is_final":input.is_final,"current":current,"context":context,"pending_transcript":input.pending_transcript,"agent_state":input.agent_state,"media":media,"questions":input.questions},"model_outputs":model_outputs,"decision":decision,"error":error,"d1_questions":decide::D1::questions(input),"d1_state":decision.map(|d|input.state_text(d.addressed_probability)).transpose()?});
    let ids: Vec<_> = std::iter::once(&input.current)
        .chain(input.context.iter())
        .map(|t| t.id.clone())
        .collect();
    writer.event(
        "d1_question",
        // A turn whose archive write is still pending has no span yet: its row is filed under Heimdall.
        current["audio"]["source_id"].as_str().unwrap_or("heimdall"),
        &ids,
        current["audio"]["end_ns"].as_u64().unwrap_or_else(capture::host_now_ns),
        payload,
    )
}
pub fn language(t: &str) -> &'static str {
    if t.chars().any(|c| "ąćęłńóśźżĄĆĘŁŃÓŚŹŻ".contains(c)) {
        "pl"
    } else {
        "en"
    }
}
fn set_login(enabled: bool) -> Result<()> {
    // The native service owns login registration; no shell-generated launch agent.
    #[cfg(target_os = "macos")]
    unsafe {
        use objc2::{msg_send, runtime::AnyObject};
        let class =
            objc2::runtime::AnyClass::get(c"SMAppService").context("SMAppService unavailable")?;
        let service: *mut AnyObject = msg_send![class, mainAppService];
        let mut error: *mut AnyObject = std::ptr::null_mut();
        let ok: bool = if enabled {
            msg_send![service,registerAndReturnError:&mut error]
        } else {
            msg_send![service,unregisterAndReturnError:&mut error]
        };
        ensure!(
            ok,
            "login registration failed; run from signed Heimdall.app"
        );
    }
    Ok(())
}
#[link(name = "ServiceManagement", kind = "framework")]
extern "C" {}

// Drawing mode (drawing.rs): a marked screenshot joins the pending attachments like a spoken screenshot's, owned
// by the conversation as it stands, so the next dispatch takes it and nothing wakes the agent.
impl Pipeline {
    pub(crate) fn attachment_room(&self) -> bool {
        self.attachment_batch.len() < 64
    }
    pub(crate) fn evidence(&self) -> &heimdall_archive::Journal {
        &self.index.evidence
    }
    pub(crate) fn island(&self, cue: ui::CommandCue) {
        self.ui(ui::UiUpdate::Command(cue));
    }
    pub(crate) fn attach_pending(&mut self, image: agent::protocol::Attachment) {
        let owner = self.tools.send_context.lock().unwrap().snapshot().through;
        self.attachment_stamps.push((owner, capture::host_now_ns()));
        self.attachment_batch.push(image);
    }
    /// Drops every pending attachment captured more than `ATTACHMENT_TTL_NS` before `now_ns`: a screenshot waits
    /// two minutes for the words it belongs to and never rides a later, unrelated wake (E9.2).
    pub(super) fn expire_attachments(&mut self, now_ns: u64) {
        let mut kept = vec![];
        let mut stamps = vec![];
        let batch = std::mem::take(&mut self.attachment_batch);
        for (image, stamp) in batch.into_iter().zip(std::mem::take(&mut self.attachment_stamps)) {
            if now_ns.saturating_sub(stamp.1) <= ATTACHMENT_TTL_NS {
                kept.push(image);
                stamps.push(stamp);
            }
        }
        self.attachment_batch = kept;
        self.attachment_stamps = stamps;
    }
}
pub(crate) const ATTACHMENT_TTL_NS: u64 = 120 * 1_000_000_000;

#[cfg(test)]
#[path = "tests/unit/mod.rs"]
mod tests;
#[cfg(test)]
#[path = "tests/plumbing_tests.rs"]
mod plumbing_tests;
#[cfg(test)]
#[path = "tests/start_latency_tests.rs"]
mod start_latency_tests;
#[cfg(test)]
#[path = "tests/resilience_tests.rs"]
mod resilience_tests;
#[cfg(test)]
#[path = "tests/listen_night2_tests.rs"]
mod listen_night2_tests;
#[cfg(test)]
#[path = "tests/delivery_night3_tests.rs"]
mod delivery_night3_tests;
#[cfg(test)]
#[path = "tests/speech_policy_tests.rs"]
mod speech_policy_tests;
#[cfg(test)]
#[path = "tests/ack_v3_tests.rs"]
mod ack_v3_tests;
#[cfg(test)]
#[path = "tests/policy_night1022_tests.rs"]
mod policy_night1022_tests;
#[cfg(test)]
#[path = "tests/switch_independence_tests.rs"]
mod switch_independence_tests;
#[cfg(test)]
#[path = "tests/provider_sign_in_tests.rs"]
mod provider_sign_in_tests;
#[cfg(test)]
#[path = "tests/drop_tests.rs"]
mod drop_tests;
#[cfg(test)]
#[path = "tests/listen_to_tests.rs"]
mod listen_to_tests;
#[cfg(test)]
#[path = "tests/heimdall_voice_tests.rs"]
mod heimdall_voice_tests;
#[cfg(test)]
#[path = "tests/heimdall_voice_replay.rs"]
mod heimdall_voice_replay;
#[cfg(test)]
#[path = "tests/vella_tests.rs"]
mod vella_tests;
#[cfg(test)]
#[path = "tests/first_run_greeting_tests.rs"]
mod first_run_greeting_tests;
#[cfg(test)]
#[path = "tests/hotkeys_tests.rs"]
mod hotkeys_tests;
#[cfg(test)]
#[path = "tests/mic_voice_tests.rs"]
mod mic_voice_tests;

/// The monthly credits the menu offers for Anthropic: Max 5x and Max 20x.
fn ui_credit_choice(usd: u32) -> bool {
    matches!(usd, 100 | 200)
}
