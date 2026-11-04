//! Speaking ▸ the voice (night 1022 menu): what speaks Heimdall's replies. Local: Drama 3 (Jarvis, the big model and
//! the default), Supertonic (the small model) or Phonon S with one of its 37 English presets; or API: ElevenLabs with a
//! voice from the user's account. Every voice plays through the same queue and playback at the user's speed (Speaking ▸ Speed).
//! The public edition (`crate::edition`) has Phonon S alone: the default (Drama 3) and a saved Drama 3 or Supertonic
//! choice speak with Phonon S, since neither model is in that bundle.
//!
//! `VoiceChoice` is the saved choice (settings `speech_voice`); `Voices` is the live side the speech glue reads per
//! utterance: the route, the lazily loaded Phonon S model (dropped when another voice is chosen), the ElevenLabs
//! client and its key in the login Keychain (installed app only, like the agent providers' keys; lab builds keep it in
//! memory or read `HEIMDALL_ELEVENLABS_KEY`).
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceSource {
    #[default]
    Local,
    Api,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LocalModel {
    /// The user's voice (Jarvis) stays the default after install.
    #[default]
    Drama3,
    Supertonic,
    Phonon,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiVoice {
    pub id: String,
    pub name: String,
}

/// The saved choice. A file without it (every file before night 1022) loads as Local, Drama 3.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceChoice {
    pub source: VoiceSource,
    pub local: LocalModel,
    pub phonon_preset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elevenlabs_voice: Option<ApiVoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_voice: Option<ApiVoice>,
    /// [voices] Own Engine speaks (`own_voice`). A flag beside `source`, not a third source, so the previous build (the
    /// kept rollback) still reads the file: it ignores the unknown fields and speaks the local choice.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub own: bool,
}
impl Default for VoiceChoice {
    fn default() -> Self {
        // The public edition ships Phonon S only (docs/editions.md); Drama 3 is the personal default.
        let local = if crate::edition::PUBLIC { LocalModel::Phonon } else { LocalModel::Drama3 };
        Self { source: VoiceSource::Local, local, phonon_preset: speak::phonon::DEFAULT_PRESET.into(), elevenlabs_voice: None, own_voice: None, own: false }
    }
}

pub use ui::speech_voice::SUGGESTED_PRESETS;

/// What says the next utterance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    Jarvis,
    Supertonic,
    Phonon(String),
    ElevenLabs(String),
    /// [voices] A voice on the user's Own Engine.
    Own(String),
}

impl VoiceChoice {
    /// API without a chosen voice speaks with the local choice (the menu shows what ElevenLabs still needs).
    pub fn route(&self) -> Route {
        if self.own {
            if let Some(v) = &self.own_voice {
                return Route::Own(v.id.clone());
            }
        }
        if self.source == VoiceSource::Api {
            if let Some(v) = &self.elevenlabs_voice {
                return Route::ElevenLabs(v.id.clone());
            }
        }
        match self.local {
            LocalModel::Drama3 | LocalModel::Supertonic if crate::edition::PUBLIC => Route::Phonon(self.phonon_preset.clone()),
            LocalModel::Drama3 => Route::Jarvis,
            LocalModel::Supertonic => Route::Supertonic,
            LocalModel::Phonon => Route::Phonon(self.phonon_preset.clone()),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.phonon_preset.trim().is_empty() && self.phonon_preset.len() <= 64, "invalid Phonon preset");
        if let Some(v) = &self.elevenlabs_voice {
            ensure!(valid_voice_id(&v.id) && v.name.len() <= 128, "invalid ElevenLabs voice");
        }
        if let Some(v) = &self.own_voice {
            ensure!(!v.id.trim().is_empty() && v.id.len() <= 128 && v.name.len() <= 128, "invalid Own Engine voice");
        }
        Ok(())
    }
    /// The menu's choice applied (`ui::speech_voice::SpeechVoiceAction`); false for actions that change no setting.
    pub fn apply(&mut self, action: &ui::speech_voice::SpeechVoiceAction) -> bool {
        use ui::speech_voice::{LocalVoice, SpeechVoiceAction as A};
        let before = self.clone();
        match action {
            A::SetApi(api) => { self.own = false; self.source = if *api { VoiceSource::Api } else { VoiceSource::Local } }
            A::SetLocal(local) => {
                self.own = false;
                self.source = VoiceSource::Local;
                self.local = match local { LocalVoice::Drama3 => LocalModel::Drama3, LocalVoice::Supertonic => LocalModel::Supertonic, LocalVoice::Phonon => LocalModel::Phonon };
            }
            A::SetPreset(name) => {
                self.own = false;
                self.source = VoiceSource::Local;
                self.local = LocalModel::Phonon;
                self.phonon_preset = name.clone();
            }
            A::SetElevenLabsVoice { id, name } => {
                self.own = false;
                self.source = VoiceSource::Api;
                self.elevenlabs_voice = Some(ApiVoice { id: id.clone(), name: name.clone() });
            }
            A::SetOwnVoice { id, name } => {
                self.own = true;
                self.own_voice = Some(ApiVoice { id: id.clone(), name: name.chars().take(128).collect() });
            }
            // Its server gone, the voice chosen before it speaks again.
            A::DisconnectOwnEngine => self.own = false,
            A::RefreshElevenLabs | A::DisconnectElevenLabs | A::RefreshOwnEngine => {}
        }
        *self != before
    }
}

fn valid_voice_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ---------------------------------------------------------------------------------------------------------
// ElevenLabs.

pub const ELEVENLABS_API: &str = "https://api.elevenlabs.io";
/// 16-bit little-endian mono PCM at 24 kHz: no MP3 decoder, and no plan requirement (44.1 kHz PCM needs Pro).
const ELEVENLABS_FORMAT: &str = "pcm_24000";
const ELEVENLABS_RATE: u32 = 24_000;
/// The public edition has its own Keychain name, so it never reads the personal app's item; the personal name is unchanged.
pub(crate) const KEYCHAIN_SERVICE: &str = if crate::edition::PUBLIC { "Heimdall Alpha API provider: ElevenLabs" } else { "Heimdall API provider: ElevenLabs" };
const KEY_ENV: &str = "HEIMDALL_ELEVENLABS_KEY";

/// Lab builds' key, for the process lifetime only.
static LAB_KEY: Mutex<Option<String>> = Mutex::new(None);

/// The saved key: the environment's (lab), else the Keychain's (installed app) or memory's (lab).
pub fn saved_key() -> Option<String> {
    if let Some(key) = std::env::var(KEY_ENV).ok().and_then(|k| agent::provider::normalize_key(&k).ok()) {
        return Some(key);
    }
    if !agent::provider::is_installed_app() {
        return LAB_KEY.lock().unwrap().clone();
    }
    use security_framework::item::{ItemClass, ItemSearchOptions, SearchResult};
    let items = ItemSearchOptions::new().class(ItemClass::generic_password()).service(KEYCHAIN_SERVICE).account(ui::speech_voice::ELEVENLABS)
        .load_data(true).skip_authenticated_items(true).search().ok()?;
    items.into_iter().find_map(|i| match i { SearchResult::Data(b) => String::from_utf8(b).ok(), _ => None })
        .and_then(|k| agent::provider::normalize_key(&k).ok())
}
fn save_key(key: &str) -> Result<()> {
    if !agent::provider::is_installed_app() {
        *LAB_KEY.lock().unwrap() = Some(key.into());
        return Ok(());
    }
    delete_key()?;
    let mut options = security_framework::passwords::PasswordOptions::new_generic_password(KEYCHAIN_SERVICE, ui::speech_voice::ELEVENLABS);
    options.set_label(KEYCHAIN_SERVICE);
    security_framework::passwords::set_generic_password_options(key.as_bytes(), options).map_err(|_| anyhow::anyhow!("Could not save the key in the Keychain"))
}
fn delete_key() -> Result<()> {
    if !agent::provider::is_installed_app() {
        *LAB_KEY.lock().unwrap() = None;
        return Ok(());
    }
    use security_framework::item::{ItemClass, ItemSearchOptions};
    match ItemSearchOptions::new().class(ItemClass::generic_password()).service(KEYCHAIN_SERVICE).account(ui::speech_voice::ELEVENLABS)
        .skip_authenticated_items(true).delete() {
        Ok(()) => Ok(()),
        Err(e) if e.code() == -25300 => Ok(()),
        Err(_) => bail!("Could not remove the key from the Keychain"),
    }
}

/// ElevenLabs over HTTPS (blocking: speech runs on its own threads). `base` is the API root (a fake server in tests).
/// The blocking client owns a private tokio runtime: building or dropping it inside an async context panics, and
/// `Voices` is built inside one (`Tools::new`). So it is built on first use (always on a speech or key thread) and
/// handed to a plain thread to drop.
#[derive(Clone)]
pub struct ElevenLabs {
    base: String,
    http: Arc<std::sync::OnceLock<BlockingHttp>>,
}
struct BlockingHttp(Option<reqwest::blocking::Client>);
impl Drop for BlockingHttp {
    fn drop(&mut self) {
        if let Some(client) = self.0.take() {
            let _ = std::thread::Builder::new().name("elevenlabs-drop".into()).spawn(move || drop(client));
        }
    }
}
impl ElevenLabs {
    pub fn new(base: &str) -> Self {
        Self { base: base.trim_end_matches('/').into(), http: Arc::default() }
    }
    fn http(&self) -> &reqwest::blocking::Client {
        self.http.get_or_init(|| BlockingHttp(Some(
            reqwest::blocking::Client::builder().connect_timeout(std::time::Duration::from_secs(5)).timeout(std::time::Duration::from_secs(60))
                .build().expect("HTTP client"))))
            .0.as_ref().expect("client present until drop")
    }
    /// The account's voices (`GET /v1/voices`), in ElevenLabs' order. Also how a key is checked.
    pub fn voices(&self, key: &str) -> Result<Vec<ui::speech_voice::ApiVoice>> {
        #[derive(Deserialize)]
        struct List { voices: Vec<Item> }
        #[derive(Deserialize)]
        struct Item { voice_id: String, name: String, #[serde(default)] description: Option<String>, #[serde(default)] labels: std::collections::BTreeMap<String, serde_json::Value> }
        let response = self.http().get(format!("{}/v1/voices", self.base)).header("xi-api-key", key).send().map_err(|e| anyhow::anyhow!("ElevenLabs unreachable: {}", e.without_url()))?;
        let response = refusal(response)?;
        let list: List = response.json().context("ElevenLabs: unexpected voice list")?;
        Ok(list.voices.into_iter().filter(|v| valid_voice_id(&v.voice_id)).map(|v| {
            let labels: Vec<String> = ["accent", "description", "use_case"].iter().filter_map(|k| v.labels.get(*k)?.as_str().map(str::to_owned)).collect();
            let desc = v.description.filter(|d| !d.trim().is_empty()).unwrap_or_else(|| labels.join(", "));
            ui::speech_voice::ApiVoice { id: v.voice_id, name: v.name, desc }
        }).collect())
    }
    /// One sentence in `voice_id`, streamed (`POST /v1/text-to-speech/{voice}/stream`, 24 kHz PCM) chunk by chunk.
    pub fn stream(&self, key: &str, voice_id: &str, text: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()> {
        ensure!(valid_voice_id(voice_id), "invalid ElevenLabs voice");
        let response = self.http().post(format!("{}/v1/text-to-speech/{voice_id}/stream?output_format={ELEVENLABS_FORMAT}", self.base))
            .header("xi-api-key", key).json(&serde_json::json!({"text": text})).send()
            .map_err(|e| anyhow::anyhow!("ElevenLabs unreachable: {}", e.without_url()))?;
        let mut response = refusal(response)?;
        let mut buffer = [0u8; 4800];
        let mut odd: Option<u8> = None;
        loop {
            ensure!(!cancel(), "speech interrupted");
            let n = std::io::Read::read(&mut response, &mut buffer).context("ElevenLabs stream")?;
            if n == 0 {
                return Ok(());
            }
            let mut bytes = Vec::with_capacity(n + 1);
            bytes.extend(odd.take());
            bytes.extend_from_slice(&buffer[..n]);
            if bytes.len() % 2 == 1 {
                odd = bytes.pop();
            }
            let samples: Vec<f32> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.).collect();
            if !samples.is_empty() {
                emit(&samples, ELEVENLABS_RATE)?;
            }
        }
    }
}

/// A non-success answer as one line in ElevenLabs' words ("ElevenLabs: invalid API key"); never echoes the key.
fn refusal(response: reqwest::blocking::Response) -> Result<reqwest::blocking::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body: serde_json::Value = response.json().unwrap_or_default();
    let detail = body.pointer("/detail/message").or_else(|| body.pointer("/detail")).and_then(|d| d.as_str()).map(str::to_owned)
        .unwrap_or_else(|| match status.as_u16() { 401 => "invalid API key".into(), 429 => "rate limited".into(), _ => format!("HTTP {}", status.as_u16()) });
    bail!("ElevenLabs: {}", detail.chars().take(160).collect::<String>())
}

/// One ElevenLabs voice as a streaming voice.
struct ElevenLabsVoice<'a> { client: &'a ElevenLabs, key: String, voice: String }
impl speak::voice::StreamingVoice for ElevenLabsVoice<'_> {
    fn label(&self) -> &str { "ElevenLabs" }
    fn stream_sentence(&mut self, text: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()> {
        self.client.stream(&self.key, &self.voice, text, cancel, emit)
    }
}

/// [voices] One Own Engine voice as a streaming voice (a sentence per request).
struct OwnVoice { endpoint: crate::own_voice::Endpoint, voice: String }
impl speak::voice::StreamingVoice for OwnVoice {
    fn label(&self) -> &str { "Own Engine" }
    fn stream_sentence(&mut self, text: &str, cancel: &dyn Fn() -> bool, emit: &mut dyn FnMut(&[f32], u32) -> Result<()>) -> Result<()> {
        let (samples, rate) = crate::own_voice::speak(&self.endpoint, &self.voice, text, cancel)?;
        emit(&samples, rate)
    }
}

// ---------------------------------------------------------------------------------------------------------
// The live side.

/// The everyday presets' groups in Setup's order (the prototype's, `lab/phonon/compare.html`); Characters stay out.
const EVERYDAY_GROUPS: [&str; 4] = ["US women", "UK women", "US men", "UK men"];
/// Setup's first voice: Heimdall's default (British, firm).
const FIRST_VOICE: &str = "Edwin";

/// Setup's voice grid: every preset that is not a Characters voice, Edwin first, then by `EVERYDAY_GROUPS` (a group
/// the file adds later goes after them), each group in the file's order.
pub fn everyday(all: &[speak::phonon::Preset]) -> Vec<ui::speech_voice::VoicePreset> {
    let rank = |p: &speak::phonon::Preset| {
        if p.name == FIRST_VOICE { return 0; }
        EVERYDAY_GROUPS.iter().position(|g| *g == p.group).map_or(EVERYDAY_GROUPS.len() + 1, |i| i + 1)
    };
    let mut list: Vec<&speak::phonon::Preset> = all.iter().filter(|p| p.group != "Characters").collect();
    // Stable: the file's order within each rank.
    list.sort_by_key(|p| rank(p));
    list.into_iter().map(|p| ui::speech_voice::VoicePreset { group: p.group.clone(), name: p.name.clone(), desc: p.desc.clone() }).collect()
}

/// A local voice with presets (Phonon S; a fake in tests).
pub trait PresetVoice: speak::voice::StreamingVoice {
    fn set_preset(&mut self, name: &str) -> Result<()>;
}
impl PresetVoice for speak::phonon::Phonon {
    fn set_preset(&mut self, name: &str) -> Result<()> { self.set_voice(name) }
}
pub type PresetLoader = Box<dyn Fn(&Path) -> Result<Box<dyn PresetVoice>> + Send + Sync>;

pub struct Voices {
    choice: Mutex<VoiceChoice>,
    phonon_dir: PathBuf,
    phonon: Mutex<Option<Box<dyn PresetVoice>>>,
    load_phonon: PresetLoader,
    pub elevenlabs: ElevenLabs,
    /// ElevenLabs' listing as the menu shows it.
    listing: Mutex<ListState>,
    /// [voices] Own Engine's listing.
    own: Mutex<ListState>,
    ui: Mutex<Option<ui::Ui>>,
    key: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// Which voice models are on disk, looked up at most every `INSTALLED_EVERY` (bug note 2026-11-04T02-17-22: `route`
    /// runs per capture tick and listed the Drama 3 folder each time).
    installed: Mutex<Option<(std::time::Instant, Installed)>>,
}
/// How long a look at the models folder stands. An install or removal is seen within this.
const INSTALLED_EVERY: std::time::Duration = std::time::Duration::from_secs(30);
#[derive(Clone, Copy, Debug)]
struct Installed { phonon_presets: bool, drama3: bool }
#[derive(Default, Clone)]
struct ListState { voices: Vec<ui::speech_voice::ApiVoice>, loading: bool, problem: Option<String> }

impl Voices {
    /// The app's: Phonon S from the bundled `models/phonon-31m-en`, ElevenLabs' real API, the saved key.
    pub fn new(models: &Path) -> Self {
        Self::with(models.join(speak::phonon::MODEL_DIR), Box::new(|dir| Ok(Box::new(speak::phonon::Phonon::load(dir)?) as Box<dyn PresetVoice>)),
            ElevenLabs::new(ELEVENLABS_API), Box::new(saved_key))
    }
    pub fn with(phonon_dir: PathBuf, load_phonon: PresetLoader, elevenlabs: ElevenLabs, key: Box<dyn Fn() -> Option<String> + Send + Sync>) -> Self {
        Self { choice: Mutex::new(VoiceChoice::default()), phonon_dir, phonon: Mutex::new(None), load_phonon, elevenlabs, listing: Default::default(), own: Default::default(), ui: Mutex::new(None), key, installed: Mutex::new(None) }
    }
    pub fn attach_ui(&self, ui: Option<ui::Ui>) {
        *self.ui.lock().unwrap() = ui;
    }
    pub fn choice(&self) -> VoiceChoice {
        self.choice.lock().unwrap().clone()
    }
    /// What speaks: the choice, except that Drama 3 where its model is not installed (the public build) is Phonon S.
    pub fn route(&self) -> Route {
        let choice = self.choice.lock().unwrap();
        match choice.route() {
            Route::Jarvis if self.public() => Route::Phonon(choice.phonon_preset.clone()),
            r => r,
        }
    }
    /// The public build: the public edition always (even without its models, VM finding P-02), or Phonon S is
    /// bundled and Drama 3 is not (with neither, as in personal tests, nothing is remapped).
    pub fn public(&self) -> bool {
        crate::edition::PUBLIC || {
            let on_disk = self.installed();
            on_disk.phonon_presets && !on_disk.drama3
        }
    }
    /// Drama 3's model folder beside Phonon S's has files (the user's personal Mac; never the public build).
    pub fn drama3_installed(&self) -> bool {
        !crate::edition::PUBLIC && self.installed().drama3
    }
    fn installed(&self) -> Installed {
        let mut cached = self.installed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, on_disk)) = *cached {
            if at.elapsed() < INSTALLED_EVERY {
                return on_disk;
            }
        }
        let on_disk = Installed {
            phonon_presets: self.phonon_dir.join(speak::phonon::PRESETS_FILE).is_file(),
            drama3: self.phonon_dir.parent().is_some_and(|m| std::fs::read_dir(m.join("drama3")).is_ok_and(|mut d| d.next().is_some())),
        };
        *cached = Some((std::time::Instant::now(), on_disk));
        on_disk
    }
    /// Settings applied: a voice no longer chosen leaves memory (Phonon S, ~300 MB).
    pub fn set_choice(&self, choice: VoiceChoice) {
        if !matches!(choice.route(), Route::Phonon(_)) {
            self.phonon.lock().unwrap().take();
        }
        *self.choice.lock().unwrap() = choice;
        self.publish();
    }
    /// Phonon S is in memory now.
    pub fn phonon_loaded(&self) -> bool {
        self.phonon.try_lock().map(|p| p.is_some()).unwrap_or(true)
    }
    /// Phonon S leaves memory unless it is the voice (after the samples' ahead-of-time pass).
    pub fn release_unused(&self) {
        if !matches!(self.route(), Route::Phonon(_)) { self.phonon.lock().unwrap().take(); }
    }
    /// Bank lines are Jarvis's recordings: they play only while Jarvis is the voice.
    pub fn bank_matches(&self) -> bool {
        self.route() == Route::Jarvis
    }
    /// One sentence through a streaming route (Phonon S or ElevenLabs), resampled and stretched to `rate`.
    pub fn speak_sentence(&self, route: &Route, written: &str, plain: &str, rate: f32, cancel: &dyn Fn() -> bool,
        emit: &mut dyn FnMut(speak::SpeechPacket) -> Result<()>) -> Result<speak::Audio> {
        match route {
            Route::Phonon(preset) => {
                let mut slot = self.phonon.lock().unwrap();
                if slot.is_none() {
                    let started = std::time::Instant::now();
                    *slot = Some((self.load_phonon)(&self.phonon_dir).context("Phonon S not loaded")?);
                    eprintln!("voice: Phonon S loaded in {:.1} s", started.elapsed().as_secs_f64());
                }
                let voice = slot.as_mut().unwrap();
                voice.set_preset(preset)?;
                speak::voice::stream_sentence(voice.as_mut(), written, plain, rate, cancel, emit)
            }
            Route::ElevenLabs(id) => {
                let key = (self.key)().context("Connect ElevenLabs")?;
                let mut voice = ElevenLabsVoice { client: &self.elevenlabs, key, voice: id.clone() };
                speak::voice::stream_sentence(&mut voice, written, plain, rate, cancel, emit)
            }
            Route::Own(id) => {
                let endpoint = crate::own_voice::saved().context("Connect Own Engine")?;
                let mut voice = OwnVoice { endpoint, voice: id.clone() };
                speak::voice::stream_sentence(&mut voice, written, plain, rate, cancel, emit)
            }
            Route::Jarvis | Route::Supertonic => bail!("not a streaming voice"),
        }
    }
    /// ElevenLabs ▸ Connect: the key checked by listing the voices, then saved; the result goes to the key panel.
    pub fn connect_elevenlabs(self: &Arc<Self>, key: String) {
        let me = self.clone();
        std::thread::Builder::new().name("elevenlabs-connect".into()).spawn(move || {
            let result = agent::provider::normalize_key(&key).and_then(|key| {
                let voices = me.elevenlabs.voices(&key)?;
                save_key(&key)?;
                Ok(voices)
            });
            let answer = match result {
                Ok(voices) => {
                    *me.listing.lock().unwrap() = ListState { voices, loading: false, problem: None };
                    Ok(())
                }
                Err(e) => Err(format!("{e:#}")),
            };
            if let Some(u) = *me.ui.lock().unwrap() {
                u.send(ui::UiUpdate::ProviderConnected { provider: ui::speech_voice::ELEVENLABS.into(), result: answer });
            }
            me.publish();
        }).ok();
    }
    pub fn disconnect_elevenlabs(&self) -> Result<()> {
        delete_key()?;
        *self.listing.lock().unwrap() = ListState::default();
        self.publish();
        Ok(())
    }
    /// Lists the account's voices again (menu open, Refresh Voices); a no-op without a key.
    pub fn refresh_elevenlabs(self: &Arc<Self>) {
        let Some(key) = (self.key)() else { return self.publish() };
        {
            let mut listing = self.listing.lock().unwrap();
            if listing.loading { return; }
            listing.loading = true;
        }
        self.publish();
        let me = self.clone();
        std::thread::Builder::new().name("elevenlabs-voices".into()).spawn(move || {
            let result = me.elevenlabs.voices(&key);
            {
                let mut listing = me.listing.lock().unwrap();
                listing.loading = false;
                match result {
                    Ok(voices) => { listing.voices = voices; listing.problem = None; }
                    Err(e) => listing.problem = Some(format!("{e:#}")),
                }
            }
            me.publish();
        }).ok();
    }
    /// [voices] Own Engine ▸ Connect (the key panel's URL and optional key): the server must answer, then the endpoint is
    /// kept and its voices listed; the result goes to the key panel.
    pub fn connect_own(self: &Arc<Self>, url: String, key: String) {
        let me = self.clone();
        std::thread::Builder::new().name("own-voice-connect".into()).spawn(move || {
            let result = crate::own_voice::connect(&url, Some(&key)).and_then(|(endpoint, voices)| { crate::own_voice::save(&endpoint)?; Ok(voices) });
            let answer = match result {
                Ok(voices) => { *me.own.lock().unwrap() = ListState { voices, loading: false, problem: None }; Ok(()) }
                Err(e) => Err(format!("{e:#}")),
            };
            if let Some(u) = *me.ui.lock().unwrap() {
                u.send(ui::UiUpdate::ProviderConnected { provider: ui::speech_voice::OWN_VOICE.into(), result: answer });
            }
            me.publish();
        }).ok();
    }
    pub fn disconnect_own(&self) -> Result<()> {
        crate::own_voice::forget()?;
        *self.own.lock().unwrap() = ListState::default();
        self.publish();
        Ok(())
    }
    /// Own Engine's voices again (menu open, Refresh Voices); a no-op when not connected.
    pub fn refresh_own(self: &Arc<Self>) {
        let Some(endpoint) = crate::own_voice::saved() else { return self.publish() };
        {
            let mut own = self.own.lock().unwrap();
            if own.loading { return; }
            own.loading = true;
        }
        self.publish();
        let me = self.clone();
        std::thread::Builder::new().name("own-voice-voices".into()).spawn(move || {
            let result = crate::own_voice::voices(&endpoint);
            {
                let mut own = me.own.lock().unwrap();
                own.loading = false;
                match result {
                    Ok(voices) => { own.voices = voices; own.problem = None; }
                    Err(e) => own.problem = Some(format!("{e:#}")),
                }
            }
            me.publish();
        }).ok();
    }
    /// The menu's state.
    pub fn ui_state(&self) -> ui::speech_voice::SpeechVoiceState {
        use ui::speech_voice::{ElevenLabsState, LocalVoice, OwnVoiceState, SpeechVoiceState, VoicePreset};
        let choice = self.choice();
        let listing = self.listing.lock().unwrap().clone();
        let own = self.own.lock().unwrap().clone();
        let all = speak::phonon::presets(&self.phonon_dir).unwrap_or_default();
        // The suggested voices, one flat list (no group), then the saved preset if it is another.
        let presets: Vec<VoicePreset> = SUGGESTED_PRESETS.iter().filter_map(|n| all.iter().find(|p| p.name == *n))
            .chain(all.iter().filter(|p| p.name == choice.phonon_preset && !SUGGESTED_PRESETS.contains(&p.name.as_str())))
            .map(|p| VoicePreset { group: String::new(), name: p.name.clone(), desc: p.desc.clone() }).collect();
        SpeechVoiceState {
            api: choice.source == VoiceSource::Api && !choice.own,
            // A Drama 3 choice without its model speaks (and shows) as Phonon S.
            local: match choice.local { LocalModel::Drama3 if !self.public() => LocalVoice::Drama3, LocalModel::Drama3 => LocalVoice::Phonon,
                // As `route()`: the edition decides, so a personal Mac without Drama 3 still shows Supertonic.
                LocalModel::Supertonic if crate::edition::PUBLIC => LocalVoice::Phonon,
                LocalModel::Supertonic => LocalVoice::Supertonic, LocalModel::Phonon => LocalVoice::Phonon },
            preset: choice.phonon_preset.clone(),
            presets,
            everyday: everyday(&all),
            elevenlabs: ElevenLabsState {
                connected: (self.key)().is_some(),
                voices: listing.voices,
                loading: listing.loading,
                problem: listing.problem,
                selected: choice.elevenlabs_voice.clone().map(|v| (v.id, v.name)),
            },
            own: choice.own && choice.own_voice.is_some(),
            // Drama 3's assets beside Phonon S's in the models folder (the user's Mac; not in the public build).
            drama3_installed: self.drama3_installed(),
            own_engine: OwnVoiceState {
                endpoint: crate::own_voice::saved().map(|e| e.base),
                voices: own.voices,
                loading: own.loading,
                problem: own.problem,
                selected: choice.own_voice.map(|v| (v.id, v.name)),
            },
        }
    }
    /// Every Phonon S preset's name (Personalise: the agent may pick one outside the suggested four).
    pub fn preset_names(&self) -> Vec<String> {
        speak::phonon::presets(&self.phonon_dir).unwrap_or_default().into_iter().map(|p| p.name).collect()
    }
    pub fn publish(&self) {
        if let Some(u) = *self.ui.lock().unwrap() {
            u.send(ui::UiUpdate::SpeechVoice(self.ui_state()));
        }
    }
}

#[cfg(test)]
#[path = "speech_voice_tests.rs"]
pub(crate) mod tests;
