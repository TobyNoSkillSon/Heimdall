use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveFormat {
    #[default]
    NativeLossless,
    Flac24,
    Flac16kMono,
}
/// Recording resolution per display.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenResolution {
    #[default]
    HalfNative,
    P540,
    P720,
    P1080,
    Native,
}
/// Continuous recording choices. Legacy history/RAM fields are ignored on load.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenSettings {
    pub resolution: ScreenResolution,
    pub fps: u32,
}
impl Default for ScreenSettings {
    fn default()->Self {Self {resolution:ScreenResolution::HalfNative,fps:5}}
}
impl ScreenSettings {
    pub fn is_default(&self)->bool {*self==Self::default()}
    pub fn normalize(&mut self) {}
    /// Seeing ▸ Recording's choice; the two enums correspond one to one.
    pub fn ui(&self) -> ui::ScreenRecording {
        let resolution = match self.resolution {
            ScreenResolution::HalfNative => ui::ScreenResolution::HalfNative,
            ScreenResolution::P540 => ui::ScreenResolution::P540,
            ScreenResolution::P720 => ui::ScreenResolution::P720,
            ScreenResolution::P1080 => ui::ScreenResolution::P1080,
            ScreenResolution::Native => ui::ScreenResolution::Native,
        };
        ui::ScreenRecording { resolution, fps: self.fps }
    }
    pub fn from_ui(r: ui::ScreenRecording) -> Self {
        let resolution = match r.resolution {
            ui::ScreenResolution::HalfNative => ScreenResolution::HalfNative,
            ui::ScreenResolution::P540 => ScreenResolution::P540,
            ui::ScreenResolution::P720 => ScreenResolution::P720,
            ui::ScreenResolution::P1080 => ScreenResolution::P1080,
            ui::ScreenResolution::Native => ScreenResolution::Native,
        };
        Self { resolution, fps: r.fps }
    }
    pub fn config(&self,_displays:&[(u32,u32)])->screen::ScreenConfig {
        let (max_width,max_height)=match self.resolution {ScreenResolution::P540=>(960,540),ScreenResolution::P720=>(1280,720),ScreenResolution::P1080=>(1920,1080),_=>(16384,16384)};
        screen::ScreenConfig {fps:self.fps,half_native:self.resolution==ScreenResolution::HalfNative,max_width,max_height}
    }
}
fn is_false(b: &bool) -> bool {
    !*b
}
fn is_true(b: &bool) -> bool {
    *b
}
/// A saved microphone choice. The name labels it in the menu while it is not connected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicrophoneChoice {
    pub uid: String,
    pub name: String,
}
/// An app the user checked in App Audio: recorded whenever it plays, across relaunches. `key` is capture's
/// `AppIdentity::key` (outer app's bundle ID, else its path; unbundled: the executable path); name and path
/// label it (with its icon) while it is not playing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppChoice {
    pub key: String,
    pub name: String,
    pub path: String,
}
pub const DEFAULT_VOICE_SPEED_PERCENT: u16 = if cfg!(feature = "public") { 100 } else { 115 };
/// The acknowledgement's added quiet before it plays, and the island's arrow and dots with it (
/// 2.5 s; 2 s, "2.5 seconds is way too long"; No added wait, so 0: the ack and
/// the dots come as the wake goes), and the longest a file or the `settings` tool may set.
pub const DEFAULT_ACK_DELAY_MS: u32 = 0;
pub const MAX_ACK_DELAY_MS: u32 = 10_000;
/// App Audio's list keeps at most this many apps that are neither recorded nor playing (the oldest go).
pub const HEARD_APPS_KEPT: usize = 24;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub dry_run: bool,
    /// Independent of Listening; first launch adopts a verified external engine.
    pub engine_on: bool,
    /// The model the engine loads when on: the Engine section's last pick (30 Oct). Empty: none picked, so an engine
    /// turned on waits for one (`ui::EngineState::Waiting`) rather than loading anything by itself.
    pub engine_model: String,
    /// The precision (quantization) chosen per engine model (model id → quant). A model without an entry, or with one
    /// the catalog no longer offers (the old `UD-Q4_K_M` of qwen3.8-27b), serves its default (`inference_engine::Model::chosen`).
    /// Written only when set.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engine_quant: std::collections::BTreeMap<String, String>,
    /// Inference Engine ▸ the thinking selector per local model (model id → Pi level; missing: Low).
    pub engine_thinking: std::collections::BTreeMap<String, String>,
    /// The agent's model: the local engine's, or one of `api_models`. Default: Claude Haiku 5.5 on OpenCode Go. Only the user
    /// changes it (a confirmed pick, or his words through the `settings` tool or an island question): nothing switches
    /// it by itself (30 Oct 01:2x), not the engine starting, a model missing, a provider connected or one removed.
    pub agent_model: agent::provider::AgentModel,
    /// Inference Engine ▸ API Providers ▸ the added models, with the metadata Pi needs offline.
    pub api_models: Vec<agent::provider::ApiModel>,
    /// The Claude models were pre-added for Anthropic once (a profile from before the provider gets them too).
    /// Missing (false) in files written before it, so those migrate once.
    #[serde(default)]
    pub anthropic_models_added: bool,
    /// Anthropic ▸ Monthly Credits: the API credit the spend estimate counts against, in US dollars ($100 with Max
    /// 5x, $200 with Max 20x; $100 until the user says which plan).
    pub anthropic_credit_usd: u32,
    /// The day of the month the credit renews (1-28). The user's plan date is not known yet; the calendar month until then.
    pub anthropic_credit_day: u8,
    /// Durable stop epoch; preserves notice cancellation across relaunch.
    pub delivery_generation: u64,
    pub archive_format: ArchiveFormat,
    pub archive_choice_pending: bool,
    /// The name friends see for this Mac after pairing (Friends). Blank means the macOS account's full name.
    pub friends_name: String,
    /// Zero keeps sessions until the user sets a retention period. Disk guard still applies.
    pub retention_days: u32,
    pub free_space_floor_bytes: u64,
    pub speak_replies: bool,
    /// Voice speed in percent of natural (Speaking ▸ Speed, 80 to 160). Both voices are time-stretched on the way
    /// to the speaker without pitch change (`speak::stretch`).
    pub voice_speed_percent: u16,
    /// Hidden: how long the user has been quiet (since his last voiced audio) before the short spoken acknowledgement
    /// ("On it.") plays and the island's send arrow and thinking dots show, in milliseconds (
    /// 2.5 s; 2 s, the animation too; 0 by default; `pipeline::ack`). Never
    /// delays the wake itself. Live from settings.json and the `settings` tool's `ack_delay`.
    pub ack_delay_ms: u32,
    /// Hidden: "standard" or "fast" (Standard/Fast speed; the `settings` tool's `agent_speed`). Fast
    /// asks OpenAI routes for the priority tier; Pi's extension reads this per request from settings.json and logs the
    /// requested and reported tiers (`agent/runtime/service-tier.mjs`). Never changed by Heimdall on its own.
    pub agent_speed: String,
    pub launch_at_login: bool,
    /// Recorded only by the explicit setup action, never by permission preflight.
    pub setup_requested: bool,
    /// The first-run greeting (`setup_wizard::greet`): false until Setup's Done has woken the agent with it, then
    /// true for good. Missing (None, not the struct's default) in files from before it: a profile already set up
    /// (`setup_requested`) counts as greeted (`migrate`), so an upgraded profile never gets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_run_greeted: Option<bool>,
    /// Setup's Done was pressed but the greeting has not reached the agent yet (Agent off): it goes at the agent's
    /// next start. Written only when true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub first_run_greeting_due: bool,
    /// Setup ▸ T3 Code ▸ Connect was clicked: the public edition connects to T3 Code only after it.
    #[serde(skip_serializing_if = "is_false")]
    pub t3_connect_allowed: bool,
    /// Setup ▸ Coding Agents (public edition): Some(true) after Set Up (every harness works without asking and is kept
    /// so at each start), Some(false) after Leave as Is (never touched), None before the step. The personal edition
    /// ignores it and always sets them (docs/harness-autonomy.md).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_autonomy: Option<bool>,
    /// Setup ▸ Arsenal ▸ Use it: the MCP bridge inside Arsenal.app (`Contents/Helpers/arsenal-mcp`), which Heimdall's Pi
    /// extension registers as the `arsenal` server at each start (runtime/heimdall.ts). None: not used (Not now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arsenal_mcp: Option<std::path::PathBuf>,
    /// The Setup step on screen while the first Setup is not done (`UiAction::SetupAt`): a relaunch (the watchdog's,
    /// an update's, a crash) reopens Setup there (`setup_wizard::first_launch`). Cleared by Setup's Done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_step: Option<String>,
    /// Ignored. Per-stream switches from before App Audio, keyed by PID and generation, so they never
    /// matched a relaunched app. Still read so older files load; never written again.
    #[serde(skip_serializing)]
    pub disabled_sources: Vec<String>,
    /// App Audio ▸ apps to record. None by default: no app's process is tapped. Written only when
    /// non-empty, so a default file stays readable by builds without this field.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub recorded_apps: Vec<AppChoice>,
    /// System Audio has delivered real audio at least once (macOS has no status query for it). Lets the
    /// setup state read Granted at launch although, with no app recorded, nothing may play for hours.
    #[serde(skip_serializing_if = "is_false")]
    pub system_audio_confirmed: bool,
    /// Primary microphone; absent means the built-in one (the first-run default). Written only
    /// when set, so a default file stays readable by builds without this field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub microphone: Option<MicrophoneChoice>,
    /// Extra microphones the user switched on; none by default.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_microphones: Vec<MicrophoneChoice>,
    /// Heimdall On ▸ off: nothing is captured and the models are unloaded until the user turns it back on.
    /// Written only when true, like the fields above, so a default file stays readable by older builds.
    #[serde(skip_serializing_if = "is_false")]
    pub paused: bool,
    /// Continuous recording resolution/rate; omitted when default.
    #[serde(skip_serializing_if = "ScreenSettings::is_default")]
    pub screen: ScreenSettings,
    /// Seeing ▸ off: no screen capture; existing footage stays in the archive. Commands answer that the screen
    /// is off. Written only when true, so a default file stays readable by older builds.
    #[serde(skip_serializing_if = "is_false")]
    pub screen_off: bool,
    pub parakeet_backend: String,
    pub turn_backend: String,
    /// The one-time move of Smart Turn from the old CPU default to Metal has run (`migrate`). Missing in files
    /// written before it, so those migrate once; after that a deliberate "cpu" sticks.
    #[serde(default)]
    pub turn_backend_migrated: bool,
    /// The one-time move from screen-buffer choices (RAM ring, 1-2 fps) to recording defaults has run. Missing
    /// in files written before it, so those start recording at half resolution, 5 fps; later choices stick.
    #[serde(default)]
    pub screen_recording_migrated: bool,
    /// Drama 3 residency: speaking (default) or always. Both unload under pressure.
    pub drama3_residency: String,
    pub tts_backend: String,
    /// Baseline, ReDimNet shaders, Nemotron shaders, or both (lab-qualified choices only).
    pub speakers_backend: String,
    // ---- speech-policy (night pass 3) ----
    /// Speaking ▸ Wake Sound: a soft tick when a spoken request goes to the agent (bridge/speech_policy.rs). On by
    /// default; a file without it (older builds) loads it on.
    pub wake_sound: bool,
    /// Hidden (echo guard, 23 Oct): true keeps Apple's echo cancellation off on speakers too (the text guard stays).
    /// Off by default until the user's morning live test (review 23 Oct, finding 1: the unit has never run on this Mac);
    /// writing `"echo_cancel_off": false` into settings.json turns it on, live. Written only when false, so a file
    /// without it follows the default and a written false sticks.
    #[serde(skip_serializing_if = "is_true")]
    pub echo_cancel_off: bool,
    // ---- end speech-policy ----
    // ---- floating islands host (night 1022) ----
    /// Hidden: true stops Heimdall launching the bundled Floating Islands helper (the tools and the watcher stay).
    /// Written only when true.
    #[serde(skip_serializing_if = "is_false")]
    pub floating_islands_off: bool,
    /// Show Islands off, or ⌃⌘,: every floating island hidden at once (the helper's
    /// `visibility.json`); the islands themselves are untouched. Written only when true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub islands_hidden: bool,
    // ---- end floating islands host ----
    // ---- speech voice (night 1022 menu) ----
    /// Speaking ▸ Local or API and the voice. Missing in files before it: Local, Drama 3 (the user's voice stays).
    pub speech_voice: crate::speech_voice::VoiceChoice,
    // ---- end speech voice ----
    // ---- menu lane (22 Oct) ----
    /// Agent ▸ Model ▸ the built-in engine row's context slider, per engine model (model id → tokens); missing: the
    /// engine's whole window, as before the slider. Written only when set.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engine_context: std::collections::BTreeMap<String, u64>,
    /// Background Tasks ▸ Model: the one model every background job runs (`crate::background_model`). Missing: the
    /// old `islands_model` (moved over by `read`), else Claude Haiku 5.5 on Anthropic.
    pub background_model: crate::background_model::BackgroundModel,
    /// Background Tasks ▸ Model's thinking level per model (provider, id, level: a level that model lists); a model
    /// without an entry runs each job's own effort. Written only when one is set.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub background_efforts: Vec<crate::background_model::BackgroundEffort>,
    /// A task's own model in place of `background_model` (Background Tasks ▸ a task's Model). Missing: the edition's
    /// default (`background_model::default_task_models`: the personal edition's island agent on Claude Haiku 5.5 through
    /// T3, `ui::ISLANDS_DEFAULT`). Always written, so a task the user put back on the shared model stays there.
    pub task_models: Vec<crate::background_model::TaskModel>,
    /// The background fallback may end on the Anthropic API key, which is paid (`background_fallback`). Off by default:
    /// money is the user's call. Written only when true.
    #[serde(skip_serializing_if = "is_false")]
    pub background_fallback_paid: bool,
    /// Background Tasks' toggles: the tasks turned off (`ui::BackgroundTask::key`). Written only when one is off.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub background_off: Vec<String>,
    // ---- end menu lane ----
    /// Show Islands on Streams: every island, the notch island and the floating islands (`islands::streams`),
    /// appears in screen shares and streams. Off by default (unshared); written only when true.
    #[serde(skip_serializing_if = "is_false")]
    pub islands_on_streams: bool,
    /// Agent ▸ Model ▸ Claude Code subscription (docs/claude-relay.md): Heimdall's main agent is a Claude
    /// thread in T3 Code on the user's Anthropic subscription instead of Pi's model. Pi stays the record (context only, no
    /// turns); `agent_model` is kept for when this is off. Off by default; written only when true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub agent_relay: bool,
    /// The T3 Claude model the relay runs (Agent ▸ Model ▸ Claude Code subscription ▸ a model); `None`: the first T3 lists.
    /// Written only when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_relay_model: Option<String>,
    /// The T3 provider instance (account) the relay runs on, e.g. `claudeAgent_claude_code_2`; `None`: automatic, the
    /// eligible account whose quota resets soonest (`agent::routing`). Written only when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_relay_account: Option<String>,
    /// Add Provider ▸ Local Splash Engine: the engine is a provider like any other and On This Mac lists its models.
    /// Adding it changes nothing else (28 Oct 17:33: it never switches the agent's model). A file from before it
    /// migrates once: added when the engine was on or the agent was on the local model.
    pub engine_added: bool,
    /// Add Provider ▸ Claude Code subscription: the relay is a provider like the engine, added or
    /// not. Added, its row is in Agent ▸ Model and Add Provider no longer offers it; adding it never makes it the agent.
    /// A file from before it migrates once: added when the relay was the agent or a relay model was chosen.
    pub relay_added: bool,
    /// Background Tasks ▸ Goals has its default model (31 Oct): a file from before the Goals task gets it once, so Goals
    /// starts on `ui::ISLANDS_DEFAULT` rather than whatever the shared model is; a later pick is never overridden.
    #[serde(default)]
    pub goals_added: bool,
    /// The menu's sections folded to their headers (`ui::MenuSection::key`), kept across launches. Written only when set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub menu_collapsed: Vec<String>,
    /// Decider ▸ the precision the decider runs in: 4-bit by default, so a file without it (every
    /// file before it) runs the 4-bit. A value this build does not know reads as the default. A change hot-swaps the
    /// decider (`pipeline::decider_swap`); a failed swap writes the previous precision back.
    #[serde(deserialize_with = "lenient_precision")]
    pub decider_precision: decide::precision::Precision,
    /// Decider ▸ which trained decider runs ("Install V6"): "v6" by default, so a file without it (every
    /// file before it) runs V6; "v5" is the rollback, kept in the bundle. A value this build does not know reads as the
    /// default. A change hot-swaps the decider like a precision change; a failed swap writes the previous one back.
    #[serde(deserialize_with = "lenient_model")]
    pub decider_model: decide::precision::DeciderModel,
    /// The T3 harnesses a relay tries, in order, by driver kind (`*` the rest, `-x` never; docs/claude-relay.md).
    /// Empty: the edition's (`agent::emergency::default_order`). Written only when set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relay_order: Vec<String>,
    /// Emergency mode skips the T3 relays and offers the local engine at once. Off by default; written only when true.
    #[serde(default, skip_serializing_if = "is_false")]
    pub relay_skip: bool,
    /// Personalise (26 Oct): the assistant's name, the user's name, the wake words and the mark (`crate::identity`).
    /// Written only when it differs from Heimdall's own, so a default file stays readable by older builds.
    #[serde(skip_serializing_if = "ui::identity::Identity::is_default", deserialize_with = "lenient_identity")]
    pub identity: ui::identity::Identity,
    /// Hidden, personal edition: the bug inbox in a code checkout, where the night round works
    /// (`heimdall_night::bug_notes::default_inbox`). Unset: `bugs/inbox` in the data folder. Written only when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bug_inbox: Option<PathBuf>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            dry_run: true,
            engine_on: false,
            agent_model: agent::provider::AgentModel::default(),
            // The public edition starts blank: no pre-added models and no monthly credit (the personal defaults are
            // the owner's plan).
            api_models: if crate::edition::PUBLIC { vec![] } else { default_api_models() },
            anthropic_models_added: true,
            anthropic_credit_usd: if crate::edition::PUBLIC { 0 } else { 100 },
            anthropic_credit_day: 1,
            // No model picked yet: the Engine section's first pick sets it.
            engine_model: String::new(),
            engine_thinking: Default::default(),
            engine_quant: Default::default(),
            delivery_generation: 0,
            archive_format: ArchiveFormat::NativeLossless,
            archive_choice_pending: true,
            friends_name: String::new(),
            retention_days: 0,
            free_space_floor_bytes: 10 * 1024 * 1024 * 1024,
            speak_replies: true,
            // A little quicker than natural ("we really want the quick speech").
            voice_speed_percent: DEFAULT_VOICE_SPEED_PERCENT,
            ack_delay_ms: DEFAULT_ACK_DELAY_MS,
            agent_speed: "standard".into(),
            launch_at_login: false,
            setup_requested: false,
            first_run_greeted: Some(false),
            first_run_greeting_due: false,
            t3_connect_allowed: false,
            harness_autonomy: None,
            arsenal_mcp: None,
            setup_step: None,
            disabled_sources: vec![],
            recorded_apps: vec![],
            system_audio_confirmed: false,
            microphone: None,
            extra_microphones: vec![],
            paused: false,
            screen: ScreenSettings::default(),
            screen_off: false,
            parakeet_backend: "metal".into(),
            // FP16 Metal: 9 vs 21 ms and 0.030 vs 0.110 CPU+GPU J/decision (opt-small pass 2). The Metal data exists only
            // for the user's tuned model; the public edition ships the stock one, which runs on the CPU (docs/editions.md).
            turn_backend: if crate::edition::PUBLIC { "cpu" } else { "metal" }.into(),
            turn_backend_migrated: true,
            screen_recording_migrated: true,
            drama3_residency: "speaking".into(),
            tts_backend: "auto".into(),
            speakers_backend: "default".into(),
            // ---- speech-policy (night pass 3) ----
            wake_sound: true,
            // Ships off: on after the user's morning live test (lab/aec/REPORT.md checks 1-6), by `"echo_cancel_off": false`.
            echo_cancel_off: true,
            // ---- end speech-policy ----
            // ---- floating islands host (night 1022) ----
            floating_islands_off: false,
            islands_hidden: false,
            // ---- end floating islands host ----
            // ---- speech voice (night 1022 menu) ----
            speech_voice: Default::default(),
            engine_context: Default::default(),
            background_model: Default::default(),
            background_efforts: vec![],
            task_models: crate::background_model::default_task_models(),
            background_fallback_paid: false,
            background_off: vec![],
            // ---- end speech voice ----
            islands_on_streams: false,
            agent_relay: false,
            agent_relay_model: None,
            agent_relay_account: None,
            engine_added: false,
            relay_added: false,
            goals_added: true,
            menu_collapsed: vec![],
            decider_precision: decide::precision::Precision::DEFAULT,
            decider_model: decide::precision::DeciderModel::DEFAULT,
            relay_order: vec![],
            relay_skip: false,
            identity: Default::default(),
            bug_inbox: None,
        }
    }
}
/// A hand-edited identity that does not read or validate never stops the app: Heimdall's own takes its place, said once
/// in the log, at launch and on a live edit alike.
fn lenient_identity<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<ui::identity::Identity, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    match serde_json::from_value::<ui::identity::Identity>(value) {
        Ok(i) if i.validate().is_ok() => Ok(i),
        Ok(i) => { eprintln!("settings: identity ignored: {}", i.validate().unwrap_err()); Ok(Default::default()) }
        Err(e) => { eprintln!("settings: identity ignored: {e}"); Ok(Default::default()) }
    }
}
/// A precision this build does not offer (a hand edit, a newer build's) runs the default instead of failing the file.
fn lenient_precision<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<decide::precision::Precision, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value.as_str().and_then(decide::precision::Precision::from_key).unwrap_or_else(|| {
        eprintln!("settings: decider_precision {value} is not 16bit, 8bit or 4bit; 4-bit runs");
        decide::precision::Precision::DEFAULT
    }))
}
/// A decider version this build does not ship (a hand edit, a newer build's) runs the default instead of failing the file.
fn lenient_model<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<decide::precision::DeciderModel, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value.as_str().and_then(decide::precision::DeciderModel::from_key).unwrap_or_else(|| {
        eprintln!("settings: decider_model {value} is not v6 or v5; Decider V6 runs");
        decide::precision::DeciderModel::DEFAULT
    }))
}
/// Islands ▸ Model became Background Tasks ▸ Model: an `islands_model` carries over as the
/// background model unless one is set already. Whether the file changed.
fn migrate_islands_model(value: &mut serde_json::Value) -> bool {
    let Some(map) = value.as_object_mut() else { return false };
    let Some(old) = map.remove("islands_model") else { return false };
    map.entry("background_model").or_insert(old);
    true
}
impl Settings {
    pub fn load(path: &Path) -> Result<Self> {
        Ok(Self::read(path)?.0)
    }
    /// `load`, and a file the one-time migrations changed is saved, so the marker is on disk and a later
    /// deliberate change sticks. The live app loads its settings this way at launch.
    pub fn load_and_migrate(path: &Path) -> Result<Self> {
        let (s, migrated) = Self::read(path)?;
        if migrated {
            s.save(path)?;
        }
        Ok(s)
    }
    /// The settings and whether an existing file needed a one-time migration.
    fn read(path: &Path) -> Result<(Self, bool)> {
        let (mut s, migrated): (Self, bool) = if path.exists() {
            let mut value:serde_json::Value=serde_json::from_slice(&fs::read(path)?).context("read settings")?;
            let missing_engine=value.get("engine_on").is_none();
            // Before Add Provider ▸ Local Splash Engine the engine ran only as the agent's model.
            let before_added=value.get("engine_added").is_none();
            let before_relay_added=value.get("relay_added").is_none();
            let before_goals=value.get("goals_added").is_none();
            let renamed=migrate_islands_model(&mut value);
            // A file from before the plan lists and the island agent were two tasks (no `task_models`, always written
            // since): its Islands off stopped the plan refresher too, so it turns Plan Lists off as well, once.
            let before_tasks=value.get("task_models").is_none();
            let mut s: Self = serde_json::from_value(value).context("read settings")?;
            let mut migrated = renamed | s.migrate() | (missing_engine && s.adopt_engine(path));
            if before_tasks {
                if s.background_off.iter().any(|k| k == "islands") && !s.background_off.iter().any(|k| k == "plan") { s.background_off.push("plan".into()); }
                migrated = true;
            }
            if before_added {
                s.engine_added = s.engine_on || s.agent_model.is_local();
                // Those files kept the engine off under an API model (one model at a time); keep reading them so.
                if !s.agent_model.is_local() { s.engine_on = false; }
                migrated = true;
            }
            if before_goals {
                if !crate::edition::PUBLIC && !s.task_models.iter().any(|m| m.task == crate::background_model::Task::Goals.key()) {
                    s.task_models.push(crate::background_model::goals_default_for(crate::background_model::Task::Goals));
                }
                s.goals_added = true;
                migrated = true;
            }
            if before_relay_added {
                s.relay_added = s.agent_relay || s.agent_relay_model.is_some();
                migrated = true;
            }
            (s, migrated)
        } else {
            let mut s=Self::default();
            let migrated=s.adopt_engine(path);
            s.engine_added = s.engine_on;
            (s,migrated)
        };
        s.screen.normalize();
        s.normalize_agent_model();
        s.validate()?;
        Ok((s, migrated))
    }
    fn adopt_engine(&mut self,path:&Path)->bool {
        if !self.agent_model.is_local() { return false; }
        if let Some(data)=path.parent() {
            match crate::inference_engine::existing_model(data) {
                Ok(Some((model,quant)))=>{
                    self.engine_on=true;
                    // A running engine on a precision other than the default keeps it.
                    let default=crate::inference_engine::Config::effective(data).0.model(&model).map(|m|m.quant.clone()).ok();
                    if default.as_deref()!=Some(quant.as_str()) {self.engine_quant.insert(model.clone(),quant);}
                    self.engine_model=model;return true;
                },
                Ok(None)=>{},
                Err(e)=>eprintln!("Engine startup discovery: {e:#}"),
            }
        }
        false
    }
    /// The precision chosen for an engine model, if any (`inference_engine::Model::chosen` falls back to the default).
    pub fn engine_quant_choice(&self, id: &str) -> Option<&str> {
        self.engine_quant.get(id).map(String::as_str)
    }
    /// The local engine's thinking level for its current model.
    pub fn engine_thinking_level(&self) -> &'static str {
        agent::provider::clamp_thinking(&agent::provider::LOCAL_THINKING, self.engine_thinking.get(&self.engine_model).map_or(agent::provider::DEFAULT_THINKING, String::as_str))
    }
    /// [menu lane] The built-in engine's context budget for its current model, inside the engine's `limit` (the
    /// slider's stop nearest the choice; the whole window when nothing was chosen).
    pub fn engine_context_budget(&self, limit: u64) -> u64 {
        let steps = ui::context_steps(limit);
        match self.engine_context.get(&self.engine_model) {
            Some(want) => steps.iter().copied().min_by_key(|s| s.abs_diff(*want)).unwrap_or(limit),
            None => limit,
        }
    }
    /// The selected API model, when it is one of the added models.
    pub fn selected_api_model(&self) -> Option<&agent::provider::ApiModel> {
        let (provider, id) = self.agent_model.api()?;
        self.api_models.iter().find(|m| m.key() == (provider, id))
    }
    /// A selection whose model was removed is no choice any more: nothing is chosen, and the Agent line says "Choose a
    /// model". It used to fall back to the local engine, a switch the user never asked for (30 Oct 01:2x: "never switch the
    /// main agent's model automatically").
    pub fn normalize_agent_model(&mut self) {
        if self.agent_model.api().is_some() && self.selected_api_model().is_none() {
            self.agent_model = agent::provider::AgentModel::Unchosen;
        }
    }
    /// The menu's folded sections.
    pub fn collapsed_sections(&self) -> Vec<ui::MenuSection> {
        self.menu_collapsed.iter().filter_map(|k| ui::MenuSection::from_key(k)).collect()
    }
    /// One-time migrations of values the app wrote as old defaults. Smart Turn: there is no UI for its backend, so
    /// a saved "cpu" from before the marker is the old default, not a choice; it becomes the Metal default once.
    /// Returns whether anything changed.
    pub fn migrate(&mut self) -> bool {
        let mut changed = self.migrate_retired_providers();
        if !self.anthropic_models_added && !crate::edition::PUBLIC {
            for model in default_api_models().into_iter().filter(|m| m.provider == agent::provider::ANTHROPIC) {
                if !self.api_models.iter().any(|m| m.key() == model.key()) { self.api_models.push(model); }
            }
            self.anthropic_models_added = true;
            changed = true;
        }
        if !self.anthropic_models_added { self.anthropic_models_added = true; changed = true; }
        // Audit 27 Oct, F2: a ChatGPT model added while Pi's catalog lacked it was stored as a 128K, text-only placeholder
        // (the user's gpt-6.1-terra); it takes its added sibling's limits now, and the account's list refreshes both.
        changed |= agent::provider::repair_placeholders(&mut self.api_models);
        if self.first_run_greeted.is_none() {
            self.first_run_greeted = Some(self.setup_requested);
            changed = true;
        }
        if !self.screen_recording_migrated {
            self.screen = ScreenSettings::default();
            self.screen_recording_migrated = true;
            changed = true;
        }
        if self.turn_backend_migrated {
            return changed;
        }
        if self.turn_backend == "cpu" {
            self.turn_backend = "metal".into();
        }
        self.turn_backend_migrated = true;
        true
    }
    /// Providers retired from the menu: Own Engine's models (any OpenAI-compatible URL) become the OpenAI-compatible
    /// custom provider's, each keeping its URL; OpenRouter's are dropped with a log line (it is added again as a custom
    /// provider). A choice that named one follows its model, or falls back like a removed model. Returns whether
    /// anything changed.
    fn migrate_retired_providers(&mut self) -> bool {
        use agent::provider::{AgentModel, CUSTOM, RETIRED_OPENROUTER, RETIRED_OWN_ENGINE};
        let before = (self.api_models.len(), self.agent_model.clone(), self.background_model.clone());
        let moved = self.api_models.iter().any(|m| m.provider == RETIRED_OWN_ENGINE);
        for m in self.api_models.iter_mut().filter(|m| m.provider == RETIRED_OWN_ENGINE) {
            m.provider = CUSTOM.into();
        }
        let mut seen = std::collections::BTreeSet::new();
        self.api_models.retain(|m| {
            if m.provider == RETIRED_OPENROUTER {
                eprintln!("Settings: OpenRouter is no longer a provider; its model {} was removed (add OpenRouter as a custom provider)", m.id);
                return false;
            }
            seen.insert((m.provider.clone(), m.id.clone()))
        });
        if let AgentModel::Api { provider, .. } = &mut self.agent_model {
            if provider == RETIRED_OWN_ENGINE { *provider = CUSTOM.into(); }
        }
        if self.background_model.provider == RETIRED_OWN_ENGINE {
            self.background_model.provider = CUSTOM.into();
        } else if self.background_model.provider == RETIRED_OPENROUTER {
            eprintln!("Settings: the background model was on OpenRouter, which is no longer a provider; it is back to the default");
            self.background_model = Default::default();
        }
        moved || before != (self.api_models.len(), self.agent_model.clone(), self.background_model.clone())
    }
    pub fn validate(&self) -> Result<()> {
        let s = self;
        ensure!((1..=28).contains(&s.anthropic_credit_day), "invalid credit renewal day");
        ensure!(s.engine_model.trim() == s.engine_model && s.engine_model.len() <= 256, "invalid engine model");
        ensure!(s.engine_quant.iter().all(|(id, q)| !id.is_empty() && id.len() <= 256 && !q.is_empty() && q.len() <= 64), "invalid engine precision");
        ensure!(s.engine_thinking.values().chain(s.api_models.iter().filter_map(|m| m.thinking.as_ref()))
            .all(|l| agent::provider::THINKING_ORDER.contains(&l.as_str())), "invalid thinking level");
        ensure!(s.engine_context.values().all(|t| (8192..=4_000_000).contains(t)), "invalid engine context"); // [menu lane]
        ensure!(matches!(s.parakeet_backend.as_str(), "metal" | "coreml"), "invalid Parakeet backend");
        ensure!(matches!(s.turn_backend.as_str(), "metal" | "cpu"), "invalid turn backend");
        ensure!(matches!(s.drama3_residency.as_str(), "speaking" | "always"), "invalid Drama 3 residency");
        ensure!(
            (speak::stretch::MIN_PERCENT..=speak::stretch::MAX_PERCENT).contains(&s.voice_speed_percent),
            "invalid voice speed"
        );
        ensure!(s.ack_delay_ms <= MAX_ACK_DELAY_MS, "invalid acknowledgement delay");
        ensure!(matches!(s.agent_speed.as_str(), "standard" | "fast"), "invalid agent speed");
        ensure!(matches!(s.tts_backend.as_str(), "auto" | "metal" | "cpu"), "invalid speech backend");
        s.speech_voice.validate()?; // speech voice (night 1022 menu)
        s.identity.validate().map_err(|e| anyhow::anyhow!("invalid identity: {e}"))?;
        ensure!(matches!(s.speakers_backend.as_str(), "default" | "redimnet" | "nemotron" | "all"), "invalid speakers backend");
        ensure!(
            s.free_space_floor_bytes >= 1024 * 1024 * 1024,
            "free-space floor must be at least 1 GiB"
        );
        ensure!(
            (1..=30).contains(&s.screen.fps),
            "invalid screen recording settings"
        );
        Ok(())
    }
    /// These fields are compatibility inputs, superseded by the user's permanent tiered archive rule.
    pub fn normalize_archive(&mut self) {
        self.archive_format = ArchiveFormat::NativeLossless;
        self.archive_choice_pending = false;
        self.retention_days = 0;
        self.disabled_sources.clear();
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        private_dir(path.parent().context("settings parent")?)?;
        let temp = path.with_extension("json.tmp");
        private_write(&temp, &serde_json::to_vec_pretty(self)?)?;
        fs::rename(temp, path)?;
        Ok(())
    }
    /// The keys capture taps (besides Heimdall's own output).
    pub fn recorded_app_keys(&self) -> Vec<String> {
        self.recorded_apps.iter().map(|a| a.key.clone()).collect()
    }
    /// App Audio ▸ an app checked or unchecked. `heard` supplies its name and path; an unchecked app that has
    /// not played this session joins `heard`, so its row stays in the menu and can be checked again.
    pub fn set_app_recorded(&mut self, key: &str, on: bool, heard: &mut Vec<HeardApp>) {
        let saved = self.recorded_apps.iter().position(|a| a.key == key);
        match (on, saved) {
            (true, None) => {
                if let Some(h) = heard.iter().find(|h| h.app.key == key) {
                    self.recorded_apps.push(AppChoice {
                        key: h.app.key.clone(),
                        name: h.app.name.clone(),
                        path: h.app.path.clone(),
                    });
                }
            }
            (false, Some(i)) => {
                let a = self.recorded_apps.remove(i);
                if !heard.iter().any(|h| h.app.key == a.key) {
                    let app = capture::AppIdentity { key: a.key, name: a.name, path: a.path };
                    heard.insert(0, HeardApp { app, playing: false });
                }
            }
            _ => {}
        }
    }
    /// The App Audio menu: every app heard this session, plus recorded apps not heard yet.
    pub fn app_audio_ui(&self, heard: &[HeardApp]) -> ui::AppAudio {
        let recorded = |key: &str| self.recorded_apps.iter().any(|a| a.key == key);
        let mut apps: Vec<ui::App> = heard
            .iter()
            .map(|h| ui::App {
                id: h.app.key.clone(),
                name: h.app.name.clone(),
                icon_path: Some(h.app.path.clone()),
                playing: h.playing,
                recorded: recorded(&h.app.key),
            })
            .collect();
        for a in &self.recorded_apps {
            if !apps.iter().any(|x| x.id == a.key) {
                apps.push(ui::App {
                    id: a.key.clone(),
                    name: a.name.clone(),
                    icon_path: Some(a.path.clone()),
                    playing: false,
                    recorded: true,
                });
            }
        }
        ui::AppAudio { apps }
    }
    /// The choices capture needs: (primary UID or built-in, extra UIDs).
    pub fn microphone_uids(&self) -> (Option<String>, Vec<String>) {
        (
            self.microphone.as_ref().map(|m| m.uid.clone()),
            self.extra_microphones.iter().map(|m| m.uid.clone()).collect(),
        )
    }
    /// Microphone ▸ primary. Choosing the built-in mic stores the default (absent).
    pub fn choose_microphone(&mut self, uid: &str, seen: &capture::MicrophoneState) {
        let device = seen.devices.iter().find(|d| d.uid == uid);
        self.extra_microphones.retain(|m| m.uid != uid);
        self.microphone = match device {
            Some(d) if d.built_in => None,
            Some(d) => Some(MicrophoneChoice { uid: d.uid.clone(), name: d.name.clone() }),
            None => self.microphone.clone().filter(|m| m.uid == uid),
        };
    }
    /// Microphone ▸ an extra mic switched on or off.
    pub fn set_microphone_on(&mut self, uid: &str, on: bool, seen: &capture::MicrophoneState) {
        let saved = self.extra_microphones.iter().position(|m| m.uid == uid);
        match (on, saved) {
            (true, None) => {
                if let Some(d) = seen.devices.iter().find(|d| d.uid == uid) {
                    self.extra_microphones.push(MicrophoneChoice { uid: d.uid.clone(), name: d.name.clone() });
                }
            }
            (false, Some(i)) => {
                self.extra_microphones.remove(i);
            }
            _ => {}
        }
    }
    /// The Microphone menu's data: what capture sees, plus saved choices that are not connected.
    pub fn microphones_ui(&self, seen: &capture::MicrophoneState) -> ui::Microphones {
        let mut devices: Vec<ui::Microphone> = seen
            .devices
            .iter()
            .map(|d| ui::Microphone { id: d.uid.clone(), name: d.name.clone(), connected: true, built_in: d.built_in })
            .collect();
        for saved in self.microphone.iter().chain(&self.extra_microphones) {
            if !devices.iter().any(|d| d.id == saved.uid) {
                devices.push(ui::Microphone {
                    id: saved.uid.clone(),
                    name: saved.name.clone(),
                    connected: false,
                    built_in: false,
                });
            }
        }
        ui::Microphones {
            devices,
            primary: self.microphone.as_ref().map(|m| m.uid.clone()),
            in_use: seen.in_use.clone(),
            extras: self.extra_microphones.iter().map(|m| m.uid.clone()).collect(),
        }
    }
}
/// An app heard this session (it played sound), and whether it is playing now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeardApp {
    pub app: capture::AppIdentity,
    pub playing: bool,
}
/// Merge capture's "playing now" report into the session's list: playing apps are marked (new ones join at
/// the end, most recent last), the rest stay listed as not playing. Capture restarts (Heimdall off/on, a
/// format change) start a fresh report, so the list lives here. Bounded: apps neither recorded nor playing
/// beyond `HEARD_APPS_KEPT` drop out, oldest first.
pub fn merge_heard(heard: &mut Vec<HeardApp>, playing: &[capture::AppIdentity], recorded: &[String]) {
    for h in heard.iter_mut() {
        h.playing = false;
    }
    for app in playing {
        heard.retain(|h| h.app.key != app.key);
        heard.push(HeardApp { app: app.clone(), playing: true });
    }
    let idle = |h: &HeardApp| !h.playing && !recorded.contains(&h.app.key);
    let mut excess = heard.iter().filter(|h| idle(h)).count().saturating_sub(HEARD_APPS_KEPT);
    heard.retain(|h| {
        if excess > 0 && idle(h) {
            excess -= 1;
            false
        } else {
            true
        }
    });
}
/// Heimdall's data folder: `<home>/.heimdall`, or the profile the app or its parent named (docs/home.md).
pub fn default_data() -> Result<PathBuf> {
    Ok(heimdall_home::data_dir())
}
/// The user's own profile folder, or None when this run was started on a named folder (`--data-dir`,
/// `HEIMDALL_DATA`: lab runs, tests). Checks of "is this the real profile" use this, not `default_data`, which is this
/// run's folder whatever named it.
pub fn profile_data() -> Option<PathBuf> {
    profile_of(heimdall_home::current())
}
fn profile_of(loc: &heimdall_home::Location) -> Option<PathBuf> {
    (loc.source != heimdall_home::Source::Env).then(|| loc.data.clone())
}

#[cfg(test)]
mod profile_tests {
    use heimdall_home::{Location, Source};
    /// Review, 27 Oct: a run on a named folder is never the user's profile, so its relay CLI, islands, engine log and
    /// Vella socket stay its own.
    #[test]
    fn a_named_folder_is_never_the_users_profile() {
        let at = |source| Location { home: "/h".into(), data: "/d".into(), source };
        assert_eq!(super::profile_of(&at(Source::Env)), None);
        for source in [Source::Pointer, Source::Legacy, Source::Fresh] {
            assert_eq!(super::profile_of(&at(source)).as_deref(), Some(std::path::Path::new("/d")));
        }
    }
}
/// Atomic menu saves and external replacements are detected by metadata. Failed parses
/// are reported once per revision; deletion never resets a running app to defaults.
#[derive(Default)]
pub struct SettingsWatch {
    revision: Option<(std::time::SystemTime, u64, u64)>,
}
impl SettingsWatch {
    pub fn read_changed(&mut self, path: &Path) -> Result<Option<Settings>> {
        let metadata = match fs::metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        #[cfg(unix)]
        let inode={use std::os::unix::fs::MetadataExt;metadata.ino()};
        #[cfg(not(unix))]
        let inode=0;
        let revision = (metadata.modified()?, metadata.len(), inode);
        if self.revision == Some(revision) { return Ok(None); }
        self.revision = Some(revision);
        Settings::load(path).map(Some)
    }
}
pub fn private_dir(p: &Path) -> Result<()> {
    fs::create_dir_all(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
pub fn private_write(p: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut o = fs::OpenOptions::new();
    o.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(p)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// The pre-added API models: Claude Haiku 5.5 on OpenCode Go, then Claude Opus 5.5, Sonnet 5.5 and Haiku 5.5 on
/// Anthropic ("integrate Anthropic API so that we can actually use like Opus directly").
pub fn default_api_models() -> Vec<agent::provider::ApiModel> {
    use agent::provider::{ApiModel, HAIKU, OPUS, SONNET};
    let mut models = vec![ApiModel::haiku()];
    models.extend([OPUS, SONNET, HAIKU].into_iter().filter_map(ApiModel::anthropic));
    models
}

#[cfg(test)]
mod control_tests {
    use super::*;
    /// Audit 27 Oct, F2: the user's settings.json kept gpt-6.1-terra as a 128K, no-output, text-only placeholder (its slider
    /// had one stop). Loading repairs it from Sol, keeps his choices, and saves once.
    #[test]
    fn the_stored_terra_placeholder_is_repaired_on_load() {
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        std::fs::write(&path,br#"{"agent_model":{"api":{"provider":"openai","id":"gpt-6.1-terra"}},"anthropic_models_added":true,"api_models":[
            {"provider":"openai","id":"gpt-6.1-sol","name":"GPT-6.1 Sol","api":"openai-responses","context":272000,"max_output":128000,"images":true,"reasoning":true,"context_budget":272000,"thinking":"high"},
            {"provider":"openai","id":"gpt-6.1-terra","name":"GPT-6.1 Terra","api":"openai-responses","context":128000,"max_output":0,"images":false,"reasoning":true,"thinking":"high"}]}"#).unwrap();
        let s=Settings::load_and_migrate(&path).unwrap();
        let terra=s.selected_api_model().unwrap();
        assert_eq!((terra.context,terra.max_output,terra.images,terra.thinking.as_deref(),terra.context_steps().len()),(272_000,128_000,true,Some("high"),3));
        assert_eq!(s.api_models[0].budget(),272_000,"Sol untouched");
        let saved:serde_json::Value=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["api_models"][1]["context"],272_000,"the repair is saved");
    }
    #[test]
    #[cfg_attr(feature = "public", ignore = "the personal edition's pre-added models; public starts blank")]
    fn haiku_is_preselected_for_existing_profiles_and_selection_persists() {
        use agent::provider::{AgentModel, ApiModel};
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        // The user's profile has no agent model yet and the local engine on: Haiku is chosen, the engine stops.
        std::fs::write(&path,br#"{"engine_on":true,"dry_run":false}"#).unwrap();
        let s=Settings::load(&path).unwrap();
        assert_eq!(s.agent_model,AgentModel::default());
        assert_eq!(s.selected_api_model(),Some(&ApiModel::haiku()));
        assert!(!s.engine_on);
        // A local choice and a removed list persist as chosen.
        let mut s=Settings::default();s.agent_model=AgentModel::Local;s.api_models.clear();s.engine_on=true;s.save(&path).unwrap();
        let back=Settings::load(&path).unwrap();
        assert_eq!((back.agent_model,back.api_models.len(),back.engine_on),(AgentModel::Local,0,true));
        // A selection whose model is no longer in the list leaves nothing chosen: never a switch to another model
        // (30 Oct 01:2x; it fell back to the local engine before).
        let mut s=Settings::default();s.api_models.clear();s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().agent_model,AgentModel::Unchosen);
        // No credential ever enters the file.
        assert!(!std::fs::read_to_string(&path).unwrap().contains("sk-"));
    }
    #[test]
    /// Part 0 (30 Oct 01:15): the relay is added or not. A file from before it reads as added when the relay was the
    /// agent or a relay model was chosen, once, and says so on disk; the menu's folded sections are kept as written.
    fn relay_added_migrates_once_and_sections_persist() {
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        let load=|bytes:&[u8]| { std::fs::write(&path,bytes).unwrap(); Settings::load_and_migrate(&path).unwrap() };
        let saved=|| serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(load(br#"{"agent_relay":true}"#).relay_added);
        assert!(load(br#"{"agent_relay_model":"claude-opus-5-5"}"#).relay_added);
        assert_eq!(saved()["relay_added"],true,"the migration is saved");
        assert!(!load(br#"{"engine_on":false}"#).relay_added);
        assert!(!load(br#"{"relay_added":false,"agent_relay_model":"claude-opus-5-5"}"#).relay_added, "a written answer stands");
        let s=load(br#"{"menu_collapsed":["agent","engine","nonsense"]}"#);
        assert_eq!(s.collapsed_sections(),[ui::MenuSection::Agent,ui::MenuSection::Engine]);
        let mut s=Settings::default();s.save(&path).unwrap();
        assert!(saved().get("menu_collapsed").is_none(),"nothing folded: not written");
        s.menu_collapsed=vec!["background".into()];s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().collapsed_sections(),[ui::MenuSection::Background]);
    }
    /// Decider precision (30 Oct): a file without it runs the 4-bit; a pick is written and read back; a value this build
    /// does not know reads as 4-bit rather than failing the file.
    #[test]
    fn decider_precision_defaults_to_4_bit_and_persists() {
        use decide::precision::Precision;
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        let load=|bytes:&[u8]| { std::fs::write(&path,bytes).unwrap(); Settings::load_and_migrate(&path).unwrap() };
        assert_eq!(load(br#"{"engine_on":false,"wake_sound":true}"#).decider_precision, Precision::Int4, "an existing file without it");
        assert_eq!(Settings::default().decider_precision, Precision::Int4);
        assert_eq!(load(br#"{"decider_precision":"8bit"}"#).decider_precision, Precision::Int8);
        assert_eq!(load(br#"{"decider_precision":"2bit","wake_sound":false}"#).decider_precision, Precision::Int4);
        assert!(!Settings::load(&path).unwrap().wake_sound, "the rest of the file still reads");
        assert_eq!(load(br#"{"decider_precision":16}"#).decider_precision, Precision::Int4);
        let mut s=Settings::default();s.decider_precision=Precision::Bf16;s.save(&path).unwrap();
        let saved=serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["decider_precision"],"16bit");
        assert_eq!(Settings::load(&path).unwrap().decider_precision, Precision::Bf16);
    }
    /// Decider version ("Install V6"): a file without it runs V6; "v5" is the rollback; a value this build
    /// does not know reads as V6 rather than failing the file.
    #[test]
    fn decider_model_defaults_to_v6_and_v5_is_the_rollback() {
        use decide::precision::DeciderModel;
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        let load=|bytes:&[u8]| { std::fs::write(&path,bytes).unwrap(); Settings::load_and_migrate(&path).unwrap() };
        assert_eq!(load(br#"{"decider_precision":"4bit","wake_sound":true}"#).decider_model, DeciderModel::V6, "User's file today: no field, V6");
        assert_eq!(Settings::default().decider_model, DeciderModel::V6);
        assert_eq!(load(br#"{"decider_model":"v5"}"#).decider_model, DeciderModel::V5);
        assert_eq!(load(br#"{"decider_model":"v9","wake_sound":false}"#).decider_model, DeciderModel::V6);
        assert!(!Settings::load(&path).unwrap().wake_sound, "the rest of the file still reads");
        assert_eq!(load(br#"{"decider_model":5}"#).decider_model, DeciderModel::V6);
        let mut s=Settings::default();s.decider_model=DeciderModel::V5;s.save(&path).unwrap();
        let saved=serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["decider_model"],"v5");
        assert_eq!(Settings::load(&path).unwrap().decider_model, DeciderModel::V5);
    }
    /// Add Provider ▸ Local Splash Engine (28 Oct): a file from before it lists the engine when it ran or served the
    /// agent, once, and says so on disk; a file written since keeps what it says, and the tile's engine stays on under
    /// an API model.
    #[test]
    fn engine_added_migrates_once_and_persists() {
        use agent::provider::AgentModel;
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        let load=|bytes:&[u8]| { std::fs::write(&path,bytes).unwrap(); Settings::load_and_migrate(&path).unwrap() };
        let saved=|| serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        let s=load(br#"{"agent_model":"local","engine_on":false}"#);
        assert!(s.engine_added && s.agent_model==AgentModel::Local);
        assert_eq!(saved()["engine_added"],true,"the migration is saved");
        let s=load(br#"{"engine_on":false}"#);
        assert!(!s.engine_added);
        assert_eq!(saved()["engine_added"],false);
        let s=load(br#"{"engine_on":true}"#);
        assert!(s.engine_added && !s.engine_on, "an old file under an API model reads as before: the engine off");
        // Since: the tile's engine under an API model stays on, and a removed engine stays removed.
        let s=load(br#"{"engine_on":true,"engine_added":true}"#);
        assert!(s.engine_added && s.engine_on && !s.agent_model.is_local());
        let s=load(br#"{"engine_on":false,"engine_added":false}"#);
        assert!(!s.engine_added);
        // The relay model: written only when set, read back as written.
        assert!(saved().get("agent_relay_model").is_none());
        let mut s=Settings::default();s.agent_relay_model=Some("claude-opus-5-5".into());s.engine_added=true;s.save(&path).unwrap();
        let back=Settings::load(&path).unwrap();
        assert_eq!((back.agent_relay_model.as_deref(),back.engine_added),(Some("claude-opus-5-5"),true));
    }
    #[test]
    #[cfg_attr(feature = "public", ignore = "the personal edition's pre-added models; public starts blank")]
    fn anthropic_models_are_added_once_to_existing_profiles() {
        let d=tempfile::tempdir().unwrap();let path=d.path().join("settings.json");
        // A profile from the OpenCode build: Haiku on OpenCode only, with a moved slider elsewhere kept.
        std::fs::write(&path,br#"{"agent_model":{"api":{"provider":"opencode-go","id":"claude-haiku-5-5"}},"api_models":[{"provider":"opencode-go","id":"claude-haiku-5-5","name":"Claude Haiku 5.5","api":"anthropic-messages","context":1000000,"max_output":128000,"price_tier_above":100000}]}"#).unwrap();
        let s=Settings::load(&path).unwrap();
        let keys: Vec<(String,String)>=s.api_models.iter().map(|m|(m.provider.clone(),m.id.clone())).collect();
        assert_eq!(keys.iter().map(|(p,i)|format!("{p}/{i}")).collect::<Vec<_>>(),["opencode-go/claude-haiku-5-5","anthropic/claude-opus-5-5","anthropic/claude-sonnet-5-5","anthropic/claude-haiku-5-5"]);
        assert_eq!((s.anthropic_credit_usd,s.anthropic_credit_day),(100,1));
        // The selection is untouched until a key is connected; removing a Claude model sticks.
        assert_eq!(s.agent_model.api(),Some(("opencode-go","claude-haiku-5-5")));
        let mut s=s;s.api_models.retain(|m|m.id!="claude-sonnet-5-5");s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().api_models.len(),3);
    }
    #[test]
    fn unknown_fields_are_ignored_at_every_settings_depth_and_bad_edits_do_not_reapply() {
        let dir=tempfile::tempdir().unwrap(); let path=dir.path().join("settings.json");
        fs::write(&path,r#"{"future":{},"screen":{"future":1},"microphone":{"uid":"a","name":"A","future":1},"recorded_apps":[{"key":"k","name":"K","path":"/K","future":1}],"extra_microphones":[{"uid":"b","name":"B","future":2}]}"#).unwrap();
        let mut watch=SettingsWatch::default();
        let s=watch.read_changed(&path).unwrap().unwrap();
        assert_eq!(s.screen.fps,5); assert_eq!(s.recorded_apps.len(),1);
        assert!(watch.read_changed(&path).unwrap().is_none());
        fs::write(&path,b"{").unwrap(); assert!(watch.read_changed(&path).is_err());
        assert!(watch.read_changed(&path).unwrap().is_none());
        fs::remove_file(&path).unwrap(); assert!(watch.read_changed(&path).unwrap().is_none());
        s.save(&path).unwrap(); assert!(watch.read_changed(&path).unwrap().is_some());
    }
    #[test]
    fn atomic_replacement_with_preserved_time_and_length_is_still_observed() {
        let dir=tempfile::tempdir().unwrap();let path=dir.path().join("settings.json");
        fs::write(&path,r#"{"retention_days":5}"#).unwrap();
        let time=fs::metadata(&path).unwrap().modified().unwrap();let mut watch=SettingsWatch::default();
        assert_eq!(watch.read_changed(&path).unwrap().unwrap().retention_days,5);
        let candidate=dir.path().join("candidate.json");fs::write(&candidate,r#"{"retention_days":6}"#).unwrap();
        fs::File::options().write(true).open(&candidate).unwrap().set_times(fs::FileTimes::new().set_modified(time)).unwrap();
        fs::rename(candidate,&path).unwrap();assert_eq!(watch.read_changed(&path).unwrap().unwrap().retention_days,6);
    }
    #[test]
    fn pause_and_screen_choices_persist_and_defaults_stay_absent() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert!(json.get("paused").is_none() && json.get("screen").is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings::default();
        s.paused = true;
        s.screen = ScreenSettings { resolution: ScreenResolution::P1080, fps: 2 };
        s.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert!(loaded.paused);
        assert_eq!(loaded.screen, s.screen);
        assert_eq!(ScreenSettings::from_ui(loaded.screen.ui()), s.screen);
        for resolution in [ScreenResolution::HalfNative, ScreenResolution::P540, ScreenResolution::P720, ScreenResolution::P1080, ScreenResolution::Native] {
            let choice = ScreenSettings { resolution, fps: 10 };
            assert_eq!(ScreenSettings::from_ui(choice.ui()), choice);
        }
        assert_eq!(ScreenSettings::default().ui(), ui::ScreenRecording::default());
        std::fs::write(&path, r#"{"screen_recording_migrated":true,"screen":{"resolution":"native","history_seconds":10,"fps":31}}"#).unwrap();
        assert!(Settings::load(&path).is_err());
    }
    #[test]
    fn menu_sizes_are_what_the_recorder_records() {
        // Seeing ▸ Recording names a frame size; the recorder must produce exactly that size.
        for display in [(3456, 2234), (3840, 2160), (1920, 1080), (2560, 1440), (1280, 800), (5120, 2880)] {
            for resolution in [ScreenResolution::HalfNative, ScreenResolution::P540, ScreenResolution::P720, ScreenResolution::P1080, ScreenResolution::Native] {
                let choice = ScreenSettings { resolution, fps: 5 };
                assert_eq!(choice.ui().resolution.frame_size(display), choice.config(&[]).size(display.0, display.1), "{resolution:?} on {display:?}");
            }
        }
    }
    #[test]
    fn screen_config_follows_recording_choice_without_ram_history() {
        let default=ScreenSettings::default().config(&[(3456,2234)]);
        assert_eq!(default.fps,5);assert_eq!(default.size(3456,2234),(1728,1116));
        let big=ScreenSettings{resolution:ScreenResolution::P1080,fps:2}.config(&[]);
        assert_eq!(big.size(3456,2234),(1670,1080));big.validate().unwrap();
        let native=ScreenSettings{resolution:ScreenResolution::Native,..Default::default()}.config(&[]);
        assert_eq!(native.size(3456,2234),(3456,2234));
    }
}

#[cfg(test)]
mod microphone_tests {
    use super::*;
    fn seen(devices: &[(&str, &str, bool)], in_use: Option<&str>) -> capture::MicrophoneState {
        capture::MicrophoneState {
            devices: devices
                .iter()
                .map(|(uid, name, built_in)| capture::InputDevice { uid: (*uid).into(), name: (*name).into(), built_in: *built_in, default: false })
                .collect(),
            in_use: in_use.map(Into::into),
            ..Default::default()
        }
    }
    const BUILT_IN: (&str, &str, bool) = ("BuiltInMicrophoneDevice", "MacBook Pro Microphone", true);
    const AIRPODS: (&str, &str, bool) = ("AC-3E-B1:input", "AirPods Pro", false);

    #[test]
    fn default_file_has_no_microphone_fields_and_old_files_load() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert!(json.get("microphone").is_none() && json.get("extra_microphones").is_none());
        let old: Settings = serde_json::from_str(r#"{"dry_run":true}"#).unwrap();
        assert_eq!(old.microphone_uids(), (None, vec![]));
    }

    #[test]
    fn the_old_cpu_turn_default_migrates_to_metal_once_and_a_later_choice_sticks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // A file written by an earlier build: the old default "cpu" and no marker.
        std::fs::write(&path, r#"{"dry_run":false,"turn_backend":"cpu","decide_backend":"cpu"}"#).unwrap();
        let s = Settings::load_and_migrate(&path).unwrap();
        assert_eq!((s.turn_backend.as_str(), s.turn_backend_migrated), ("metal", true));
        assert!(!s.dry_run, "agent setting survives migration");
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((written["turn_backend"].as_str(), written["turn_backend_migrated"].as_bool()), (Some("metal"), Some(true)));
        // A deliberate "cpu" afterwards (the marker is on disk) is kept, by launch and by the live reload.
        let mut chosen = s;
        chosen.turn_backend = "cpu".into();
        chosen.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(Settings::load_and_migrate(&path).unwrap().turn_backend, "cpu");
        assert_eq!(Settings::load(&path).unwrap().turn_backend, "cpu");
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing to migrate: the file is not rewritten");
        // A file without the field loads Metal; a fresh default is already migrated.
        std::fs::write(&path, r#"{"dry_run":true}"#).unwrap();
        assert_eq!(Settings::load(&path).unwrap().turn_backend, "metal");
        assert!(Settings::default().turn_backend_migrated);
        // No file: nothing is written.
        let missing = dir.path().join("none.json");
        assert_eq!(Settings::load_and_migrate(&missing).unwrap().turn_backend, "metal");
        assert!(!missing.exists());
    }

    #[test]
    fn screen_buffer_choices_migrate_to_recording_defaults_once_and_a_later_choice_sticks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // Written by the screen-buffer build, and by the first recording build before the marker existed.
        for old in [
            r#"{"turn_backend_migrated":true,"screen":{"resolution":"native","history_seconds":60,"fps":2}}"#,
            r#"{"turn_backend_migrated":true,"screen":{"resolution":"native","fps":2}}"#,
        ] {
            std::fs::write(&path, old).unwrap();
            let s = Settings::load_and_migrate(&path).unwrap();
            assert_eq!((s.screen, s.screen_recording_migrated), (ScreenSettings::default(), true));
            assert_eq!(s.turn_backend, "metal");
        }
        let mut chosen = Settings::load(&path).unwrap();
        chosen.screen.fps = 2;
        chosen.save(&path).unwrap();
        assert_eq!(Settings::load_and_migrate(&path).unwrap().screen.fps, 2);
        assert!(Settings::default().screen_recording_migrated);
    }

    #[test]
    fn choices_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let both = seen(&[BUILT_IN, AIRPODS], Some("BuiltInMicrophoneDevice"));
        let mut s = Settings::default();
        s.choose_microphone("AC-3E-B1:input", &both);
        s.set_microphone_on("BuiltInMicrophoneDevice", true, &both);
        s.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(
            loaded.microphone_uids(),
            (Some("AC-3E-B1:input".into()), vec!["BuiltInMicrophoneDevice".into()])
        );
        // Back to built-in stores the default; switching the extra off removes it.
        let mut s = loaded;
        s.choose_microphone("BuiltInMicrophoneDevice", &both);
        s.set_microphone_on("BuiltInMicrophoneDevice", false, &both);
        assert_eq!(s.microphone_uids(), (None, vec![]));
    }

    #[test]
    fn disconnected_choice_stays_in_the_menu_with_its_name() {
        let mut s = Settings::default();
        s.choose_microphone("AC-3E-B1:input", &seen(&[BUILT_IN, AIRPODS], None));
        let ui = s.microphones_ui(&seen(&[BUILT_IN], Some("BuiltInMicrophoneDevice")));
        let menu = ui.menu();
        assert_eq!(menu.primary[0].title, "MacBook Pro Microphone — In Use");
        assert_eq!(menu.primary[1].title, "AirPods Pro — Not Connected");
        assert!(menu.primary[1].on);
    }
}

#[cfg(test)]
mod app_audio_tests {
    use super::*;
    fn app(key: &str, name: &str) -> capture::AppIdentity {
        capture::AppIdentity { key: key.into(), name: name.into(), path: format!("/Applications/{name}.app") }
    }

    #[test]
    fn default_records_nothing_and_old_files_with_disabled_sources_load_but_are_never_rewritten() {
        let json = serde_json::to_value(Settings::default()).unwrap();
        assert!(json.get("recorded_apps").is_none() && json.get("system_audio_confirmed").is_none());
        assert!(json.get("disabled_sources").is_none());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"dry_run":true,"disabled_sources":["process:812:1760:4"]}"#).unwrap();
        let old = Settings::load(&path).unwrap();
        assert!(old.recorded_app_keys().is_empty(), "a PID-keyed switch never becomes an app choice");
        old.save(&path).unwrap();
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(written.get("disabled_sources").is_none());
    }

    #[test]
    fn checked_apps_persist_reload_and_uncheck() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut heard = vec![];
        merge_heard(&mut heard, &[app("com.hnc.Discord", "Discord"), app("/usr/bin/say", "say")], &[]);
        let mut s = Settings::default();
        s.set_app_recorded("com.hnc.Discord", true, &mut heard);
        s.set_app_recorded("/usr/bin/say", true, &mut heard);
        s.set_app_recorded("com.unknown", true, &mut heard); // never heard: nothing to name it by, ignored
        s.system_audio_confirmed = true;
        s.save(&path).unwrap();
        let loaded = Settings::load(&path).unwrap();
        assert_eq!(loaded.recorded_app_keys(), ["com.hnc.Discord", "/usr/bin/say"]);
        assert!(loaded.system_audio_confirmed);
        // After a relaunch nothing has played yet: the recorded apps are still listed, checked, by name.
        let ui = loaded.app_audio_ui(&[]).menu();
        let rows: Vec<_> = ui.rows.iter().map(|r| (r.title.as_str(), r.on)).collect();
        assert_eq!(rows, [("Discord", true), ("say", true)]);
        // Unchecked before it played this session: still listed, unchecked, and can be checked again.
        let mut s = loaded;
        let mut heard = vec![];
        s.set_app_recorded("com.hnc.Discord", false, &mut heard);
        assert_eq!(s.recorded_app_keys(), ["/usr/bin/say"]);
        let rows: Vec<_> = s.app_audio_ui(&heard).menu().rows.into_iter().map(|r| (r.title, r.on)).collect();
        assert_eq!(rows, [("Discord".to_string(), false), ("say".to_string(), true)]);
        s.set_app_recorded("com.hnc.Discord", true, &mut heard);
        assert_eq!(s.recorded_app_keys(), ["/usr/bin/say", "com.hnc.Discord"]);
    }

    #[test]
    fn heard_list_marks_playing_keeps_quiet_apps_and_stays_bounded() {
        let mut heard = vec![];
        merge_heard(&mut heard, &[app("a", "A"), app("b", "B")], &[]);
        merge_heard(&mut heard, &[app("b", "B")], &[]);
        let state: Vec<_> = heard.iter().map(|h| (h.app.key.as_str(), h.playing)).collect();
        assert_eq!(state, [("a", false), ("b", true)]);
        let s = Settings { recorded_apps: vec![AppChoice { key: "a".into(), name: "A".into(), path: "/A.app".into() }], ..Default::default() };
        let menu = s.app_audio_ui(&heard).menu();
        let rows: Vec<_> = menu.rows.iter().map(|r| (r.title.as_str(), r.on)).collect();
        assert_eq!(rows, [("A", true), ("B — Playing", false)]);
        // Many short-lived apps: the oldest quiet, unrecorded ones drop; recorded "a" stays.
        for i in 0..40 {
            merge_heard(&mut heard, &[app(&format!("x{i}"), "X")], &["a".to_string()]);
        }
        merge_heard(&mut heard, &[], &["a".to_string()]);
        assert_eq!(heard.len(), HEARD_APPS_KEPT + 1);
        assert!(heard.iter().any(|h| h.app.key == "a"));
        assert_eq!(heard.last().unwrap().app.key, "x39");
    }
}

#[cfg(test)]
mod primary_promotion_regression {
    #[test]
    fn choosing_an_extra_as_primary_removes_duplicate_membership() {
        let mut settings=super::Settings::default();
        settings.extra_microphones=vec![super::MicrophoneChoice {uid:"external".into(),name:"External".into()}];
        let seen=capture::MicrophoneState {devices:vec![capture::InputDevice {uid:"external".into(),name:"External".into(),built_in:false,default:false}],..Default::default()};
        settings.choose_microphone("external",&seen);
        assert_eq!(settings.microphone.as_ref().unwrap().uid,"external");assert!(settings.extra_microphones.is_empty());
    }
}

#[cfg(test)]
mod echo_cancel_settings {
    use super::*;
    /// Review 23 Oct, finding 1: echo cancellation ships off. A file without the key (every file today) loads it off;
    /// the user's morning switch-on, `"echo_cancel_off": false` written into the file, loads on and survives the app's
    /// own later saves.
    #[test]
    fn echo_cancellation_is_off_by_default_and_a_written_false_turns_it_on_and_sticks() -> Result<()> {
        assert!(Settings::default().echo_cancel_off);
        let old: Settings = serde_json::from_str(r#"{"speak_replies":true}"#)?;
        assert!(old.echo_cancel_off, "a file without the key: off");
        let t = tempfile::tempdir()?;
        let path = t.path().join("settings.json");
        Settings::default().save(&path)?;
        assert!(!fs::read_to_string(&path)?.contains("echo_cancel_off"), "the default stays out of the file");
        fs::write(&path, r#"{"speak_replies":true,"echo_cancel_off":false}"#)?;
        let on = Settings::load(&path)?;
        assert!(!on.echo_cancel_off);
        on.save(&path)?;
        assert!(!Settings::load(&path)?.echo_cancel_off, "the app's own save kept User's switch-on");
        Ok(())
    }
}

#[cfg(test)]
mod menu_redesign_settings {
    use super::*;
    #[test]
    fn seeing_persists_and_old_short_history_loads_as_twenty_seconds() -> Result<()> {
        let t = tempfile::tempdir()?;
        let path = t.path().join("settings.json");
        // Defaults stay out of the file, so older builds still read it.
        Settings::default().save(&path)?;
        let raw = fs::read_to_string(&path)?;
        assert!(!raw.contains("screen_off"));
        let s = Settings { screen_off: true, ..Default::default() };
        s.save(&path)?;
        let back = Settings::load(&path)?;
        assert!(back.screen_off);
        // 5 and 10 seconds are no longer offered: a saved choice loads as 20, without an error.
        for old in [5, 10] {
            fs::write(&path, format!(r#"{{"screen":{{"history_seconds":{old}}},"unknown_future_field":1}}"#))?;
            assert_eq!(Settings::load(&path)?.screen.fps, 5);
        }
        Ok(())
    }
    /// The retired Speaking ▸ Voice picker wrote `"voice"`; such a file still loads and validates, and the
    /// next save drops the field (Heimdall speaks in one voice).
    #[test]
    fn a_file_with_the_retired_voice_choice_loads_and_saves_without_it() -> Result<()> {
        let t = tempfile::tempdir()?;
        let path = t.path().join("settings.json");
        let mut old: serde_json::Value = serde_json::to_value(Settings { screen_off: true, ..Default::default() })?;
        old["voice"] = "M1".into();
        fs::write(&path, serde_json::to_vec_pretty(&old)?)?;
        let back = Settings::load_and_migrate(&path)?;
        back.validate()?;
        assert!(back.screen_off, "the rest of the file still applies");
        back.save(&path)?;
        let raw = fs::read_to_string(&path)?;
        assert!(!raw.contains("\"voice\""), "{raw}");
        Ok(())
    }
}

#[cfg(test)]
mod drama3_tests {
    use super::*;
    #[test]
    fn residency_defaults_to_speaking_and_round_trips_always() -> Result<()> {
        let old: Settings=serde_json::from_str("{}")?;
        assert_eq!(old.drama3_residency,"speaking");
        let temp=tempfile::tempdir()?;let path=temp.path().join("settings.json");
        let mut settings=old;settings.drama3_residency="always".into();settings.save(&path)?;
        assert_eq!(Settings::load(&path)?.drama3_residency,"always");
        settings.drama3_residency="invalid".into();settings.save(&path)?;
        assert!(Settings::load(&path).is_err());Ok(())
    }
    #[test]
    fn voice_speed_defaults_quick_persists_and_rejects_out_of_range() -> Result<()> {
        let old: Settings=serde_json::from_str("{}")?;
        assert_eq!(old.voice_speed_percent,115,"files from before the slider get the quick default");
        let temp=tempfile::tempdir()?;let path=temp.path().join("settings.json");
        let mut settings=old;settings.voice_speed_percent=140;settings.save(&path)?;
        assert_eq!(Settings::load(&path)?.voice_speed_percent,140);
        for bad in [79,161] {settings.voice_speed_percent=bad;settings.save(&path)?;assert!(Settings::load(&path).is_err());}
        Ok(())
    }
    /// The ack waits 2.5 s after he stops talking; a setting, live from settings.json; no added wait
    /// by default.
    #[test]
    fn ack_delay_defaults_to_no_wait_persists_and_rejects_out_of_range() -> Result<()> {
        let old: Settings=serde_json::from_str("{}")?;
        assert_eq!(old.ack_delay_ms,0,"files from before the setting get no added wait");
        let temp=tempfile::tempdir()?;let path=temp.path().join("settings.json");
        let mut settings=old;settings.ack_delay_ms=4_000;settings.save(&path)?;
        assert_eq!(Settings::load(&path)?.ack_delay_ms,4_000);
        settings.ack_delay_ms=MAX_ACK_DELAY_MS+1;settings.save(&path)?;assert!(Settings::load(&path).is_err());
        Ok(())
    }
}

// [providers] night 1022: the menu revamp moves the model choice into Agent ▸ Model and drops Anthropic's credit
// choice. The user's live profile (Anthropic, Claude Opus 5.5 at a 252,571-token budget and Medium thinking, the old $200
// credit) must load unchanged and keep its selection.
#[cfg(test)]
mod providers_migration_tests {
    use super::*;
    #[test]
    fn owner_live_anthropic_opus_selection_survives_the_menu_revamp() {
        use agent::provider::AgentModel;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("settings.json");
        let claude = |id: &str, name: &str, extra: &str| format!(r#"{{"provider":"anthropic","id":"{id}","name":"{name}","api":"anthropic-messages","context":1000000,"max_output":128000,"images":true,"reasoning":true,"cost":{{"input":4.0,"output":20.0,"cacheRead":0.2,"cacheWrite":5.0}}{extra}}}"#);
        let live = format!(
            r#"{{"agent_model":{{"api":{{"provider":"anthropic","id":"claude-opus-5-5"}}}},"engine_on":false,"engine_model":"qwen3.8-27b","engine_thinking":{{}},"anthropic_models_added":true,"anthropic_credit_usd":200,"anthropic_credit_day":1,"api_models":[{{"provider":"opencode-go","id":"claude-haiku-5-5","name":"Claude Haiku 5.5","api":"anthropic-messages","context":1000000,"max_output":128000}},{},{},{}]}}"#,
            claude("claude-opus-5-5", "Claude Opus 5.5", r#","context_budget":252571,"thinking":"medium""#),
            claude("claude-sonnet-5-5", "Claude Sonnet 5.5", ""),
            claude("claude-haiku-5-5", "Claude Haiku 5.5", ""),
        );
        std::fs::write(&path, &live).unwrap();
        let s = Settings::load_and_migrate(&path).unwrap();
        assert_eq!(s.agent_model, AgentModel::Api { provider: "anthropic".into(), id: "claude-opus-5-5".into() });
        let opus = s.selected_api_model().unwrap();
        assert_eq!((opus.context_budget, opus.thinking.as_deref(), opus.base_url.as_deref(), opus.unsupported()), (Some(252_571), Some("medium"), None, None));
        assert_eq!((opus.budget(), s.engine_on, s.api_models.len()), (252_571, false, 4));
        // Saved again it reads back the same, with no endpoint field on models that have none.
        s.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("base_url"));
        assert_eq!(Settings::load(&path).unwrap(), s);
        // A model on the user's own URL keeps its endpoint across a save.
        let mut with_own = s.clone();
        with_own.api_models.push(agent::provider::ApiModel { provider: agent::provider::CUSTOM.into(), id: "gemma-4-27b".into(), name: "gemma-4-27b".into(), base_url: Some("http://localhost:1234/v1".into()), cost: None, ..opus.clone() });
        with_own.save(&path).unwrap();
        let back = Settings::load(&path).unwrap();
        assert_eq!(back.api_models.last().and_then(|m| m.base_url.as_deref()), Some("http://localhost:1234/v1"));
        assert_eq!(back.agent_model, s.agent_model);
    }
}

#[cfg(test)]
mod retired_provider_tests {
    use super::*;
    use agent::provider::AgentModel;

    /// A saved Own Engine model becomes the OpenAI-compatible custom provider's with its URL (also as the agent's and
    /// the background model); an OpenRouter model is dropped with a log line; the file loads and is saved once.
    #[test]
    fn saved_openrouter_and_own_engine_settings_load() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("settings.json");
        let model = |provider: &str, id: &str, base: Option<&str>| serde_json::json!({"provider":provider,"id":id,"name":id,"api":"openai-completions","context":131072,"max_output":0,"base_url":base});
        let file = serde_json::json!({"agent_model":{"api":{"provider":"own-engine","id":"qwen3.8-32b"}},"anthropic_models_added":true,
            "background_model":{"provider":"own-engine","id":"qwen3.8-32b"},
            "api_models":[model("own-engine","qwen3.8-32b",Some("http://localhost:1234/v1")), model("openrouter","moonshotai/kimi-k3",None)]});
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let s = Settings::load_and_migrate(&path).unwrap();
        assert_eq!(s.agent_model, AgentModel::Api { provider: "openai-compatible".into(), id: "qwen3.8-32b".into() });
        assert_eq!(s.api_models.len(), 1);
        let m = s.selected_api_model().unwrap();
        assert_eq!((m.provider.as_str(), m.base_url.as_deref(), m.pi_definition().unwrap()["baseUrl"].as_str()), ("openai-compatible", Some("http://localhost:1234/v1"), Some("http://localhost:1234/v1")));
        assert_eq!(s.background_model.provider, "openai-compatible");
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("own-engine") && !saved.contains("openrouter"), "migrated once, on disk");
        // An OpenRouter choice is gone like a removed model: nothing is chosen (never a switch to the engine).
        let file = serde_json::json!({"agent_model":{"api":{"provider":"openrouter","id":"moonshotai/kimi-k3"}},"anthropic_models_added":true,
            "background_model":{"provider":"openrouter","id":"moonshotai/kimi-k3"},"api_models":[model("openrouter","moonshotai/kimi-k3",None)]});
        std::fs::write(&path, serde_json::to_vec(&file).unwrap()).unwrap();
        let s = Settings::load_and_migrate(&path).unwrap();
        assert!(s.api_models.is_empty());
        assert_eq!(s.agent_model, AgentModel::Unchosen);
        assert_eq!(s.background_model, crate::background_model::BackgroundModel::default());
    }
}

#[cfg(test)]
mod background_tasks_tests {
    use super::*;
    use crate::background_model::BackgroundModel;

    /// The user's Islands ▸ Model carries over as Background Tasks ▸ Model, once, and the file is rewritten without it.
    #[test]
    fn the_islands_model_becomes_the_background_model() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"engine_on":false,"islands_model":{"provider":"anthropic","id":"claude-sonnet-5-5"}}"#).unwrap();
        let s = Settings::load_and_migrate(&path).unwrap();
        let sonnet = BackgroundModel { provider: "anthropic".into(), id: "claude-sonnet-5-5".into() };
        assert_eq!(s.background_model, sonnet);
        let written: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(written.get("islands_model").is_none());
        assert_eq!(written["background_model"]["id"], "claude-sonnet-5-5");
        assert!(written.get("background_off").is_none(), "every task on: nothing written");
        assert_eq!(Settings::load(&path).unwrap().background_model, sonnet);
        // A file with both (an older build wrote the old field back) keeps the background model.
        std::fs::write(&path, r#"{"engine_on":false,"islands_model":{"provider":"local","id":""},"background_model":{"provider":"anthropic","id":"claude-opus-5-5"},"background_off":["voice_names"]}"#).unwrap();
        let s = Settings::load(&path).unwrap();
        assert_eq!(s.background_model.id, "claude-opus-5-5");
        assert_eq!(crate::background_model::off_tasks(&s.background_off), [ui::BackgroundTask::VoiceNames]);
    }

    /// Per-model thinking levels (Background Tasks ▸ Model): saved and loaded as they were, nothing written while none
    /// is set, and a file without the field loads none.
    #[test]
    fn background_efforts_round_trip() {
        use crate::background_model::BackgroundEffort;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings::default();
        s.save(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("background_efforts"), "none set: nothing written");
        s.background_model = BackgroundModel { provider: "t3:claudeAgent_claude_code_2".into(), id: "claude-haiku-5-5".into() };
        s.background_efforts = vec![
            BackgroundEffort { provider: "t3:claudeAgent_claude_code_2".into(), id: "claude-haiku-5-5".into(), level: "low".into() },
            BackgroundEffort { provider: "anthropic".into(), id: "claude-sonnet-5-5".into(), level: "max".into() },
        ];
        s.save(&path).unwrap();
        let back = Settings::load(&path).unwrap();
        assert_eq!((back.background_model, back.background_efforts), (s.background_model.clone(), s.background_efforts.clone()));
        let old: Settings = serde_json::from_str(r#"{"background_model":{"provider":"anthropic","id":"claude-haiku-5-5"}}"#).unwrap();
        assert!(old.background_efforts.is_empty());
        // The fallback's paid step: off unless the user turned it on, and kept when he did.
        assert!(!old.background_fallback_paid);
        let mut paid = Settings::default();
        paid.background_fallback_paid = true;
        paid.save(&path).unwrap();
        assert!(Settings::load(&path).unwrap().background_fallback_paid);
    }

    /// Review 30 Oct: Islands off used to stop the plan refresher too; a file from before the split keeps both off, once
    /// (the saved file then carries `task_models`, so turning Plan Lists back on sticks).
    #[test]
    fn an_old_islands_off_also_turns_the_plan_lists_off_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"engine_on":false,"engine_added":false,"background_off":["islands"]}"#).unwrap();
        let s = Settings::load_and_migrate(&path).unwrap();
        assert_eq!(crate::background_model::off_tasks(&s.background_off), [ui::BackgroundTask::Islands, ui::BackgroundTask::Plan]);
        let mut s = s;
        s.background_off.retain(|k| k != "plan");
        s.save(&path).unwrap();
        assert_eq!(Settings::load_and_migrate(&path).unwrap().background_off, ["islands"], "Plan Lists turned back on stays on");
        // Islands on before: nothing changes.
        std::fs::write(&path, r#"{"engine_on":false,"engine_added":false,"background_off":["voice_names"]}"#).unwrap();
        assert_eq!(Settings::load(&path).unwrap().background_off, ["voice_names"]);
    }

    /// A task's own model: a file without the field gets the edition's default (the personal
    /// island agent on Claude Haiku 5.5 through T3 on Claude code 2, beside his saved shared model); a task put back
    /// on the shared model stays there across a save and load; an unknown task key is ignored.
    #[test]
    fn task_models_default_and_round_trip() {
        use crate::background_model::{default_task_models, TaskModel};
        let old: Settings = serde_json::from_str(r#"{"background_model":{"provider":"openai","id":"gpt-6-luna"}}"#).unwrap();
        assert_eq!(old.task_models, default_task_models());
        assert_eq!(old.background_model.id, "gpt-6-luna", "his saved model stays the shared one");
        if !crate::edition::PUBLIC {
            assert_eq!(old.task_models, [
                TaskModel { task: "islands".into(), provider: "t3:claudeAgent_claude_code_2".into(), id: "claude-haiku-5-5".into() },
                TaskModel { task: "goals".into(), provider: "t3:claudeAgent_claude_code_2".into(), id: "claude-haiku-5-5".into() },
            ]);
        } else {
            assert!(old.task_models.is_empty());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let mut s = Settings::default();
        s.task_models = vec![];
        s.save(&path).unwrap();
        assert!(Settings::load(&path).unwrap().task_models.is_empty(), "back on the shared model: stays there");
        s.task_models = vec![TaskModel { task: "voice_names".into(), provider: "anthropic".into(), id: "claude-sonnet-5-5".into() }];
        s.save(&path).unwrap();
        assert_eq!(Settings::load(&path).unwrap().task_models, s.task_models);
        let odd: Settings = serde_json::from_str(r#"{"task_models":[{"task":"mining","provider":"anthropic","id":"x"}]}"#).unwrap();
        assert!(odd.task_models[0].ui().is_none(), "an unknown task is ignored");
    }

    /// Goals (31 Oct): a file from before it (the user's: the shared model on Luna, the island agent on its own) gets Goals
    /// on the default once; Goals put back on the shared model afterwards stays there.
    #[test]
    fn goals_gets_its_default_model_once() {
        if crate::edition::PUBLIC { return }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"engine_on":false,"engine_added":false,"relay_added":false,"background_model":{"provider":"openai","id":"gpt-6-luna"},"task_models":[{"task":"islands","provider":"t3:claudeAgent_claude_code_2","id":"claude-haiku-5-5"}]}"#).unwrap();
        let mut s = Settings::load_and_migrate(&path).unwrap();
        assert!(s.task_models.iter().any(|m| m.task == "goals" && m.id == "claude-haiku-5-5" && m.provider == "t3:claudeAgent_claude_code_2"));
        assert_eq!(s.background_model.id, "gpt-6-luna", "the shared model is untouched");
        s.task_models.retain(|m| m.task != "goals");
        s.save(&path).unwrap();
        assert!(!Settings::load_and_migrate(&path).unwrap().task_models.iter().any(|m| m.task == "goals"), "his choice stays");
    }
}

#[cfg(test)]
mod identity_tests {
    #[test]
    fn a_bad_identity_falls_back_to_heimdall_and_the_rest_loads() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("settings.json");
        for bad in [r#"{"identity":{"name":"Ivy!","user_name":"Evelyn"},"voice_speed_percent":120}"#, r#"{"identity":{"name":"Ivy","icon":{"kind":"emoji"}},"voice_speed_percent":120}"#] {
            std::fs::write(&p, bad).unwrap();
            let s = super::Settings::load(&p).unwrap();
            assert_eq!((s.identity.is_default(), s.voice_speed_percent), (true, 120), "{bad}");
        }
        std::fs::write(&p, r#"{"identity":{"name":"Ivy","user_name":"Evelyn","icon":{"kind":"symbol","name":"leaf"}}}"#).unwrap();
        assert_eq!(super::Settings::load(&p).unwrap().identity.name, "Ivy");
        // Heimdall's own identity is never written.
        super::Settings::default().save(&p).unwrap();
        assert!(!std::fs::read_to_string(&p).unwrap().contains("identity"));
    }
}
