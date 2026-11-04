//! The agent's `settings` tool ("the agent can change any Heimdall setting … that's kind of the
//! whole point"): every setting the menu shows, read with `get` and `list` and changed with `set`.
//!
//! A change is planned here as the very `ui::UiAction`s the menu's rows send, checked against what the menu offers
//! (the microphones and apps it lists, the added models, the voices), and then handed to the menu's own callback
//! (`McpActions::menu`, the app's `ui_action_callback`): the Agent and Listening latches, the live loop, the pipeline's
//! `action`, `apply_menu_settings` and the save all run as for a click, and the app's answer redraws the menu.
//!
//! The agent never switches itself off on its own: `agent` false (and the engine off while the agent runs on it) needs
//! `owner_asked` with his words, which must be in one of his recent lines (spoken or typed); an allowed change is logged
//! (`agent_off_check`, `Pipeline::record_agent_off`).
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use ui::UiAction;

use crate::settings::Settings;

/// What the tool reads: the settings and what the menu currently lists. Built on the pipeline thread.
#[derive(Clone, Debug)]
pub struct View {
    pub settings: Settings,
    /// The Agent tile is off (the latch, as the menu shows it).
    pub agent_off: bool,
    /// Listening is on (Heimdall On).
    pub listening: bool,
    /// The Microphone submenu's devices and choices.
    pub microphones: ui::Microphones,
    /// The App Audio submenu's apps.
    pub apps: ui::AppAudio,
    pub voices: ui::speech_voice::SpeechVoiceState,
    /// Every Phonon S preset name (the suggested ones and the rest).
    pub presets: Vec<String>,
    /// The built-in engine's models: (id, label).
    pub engine_models: Vec<(String, String)>,
    /// Agent ▸ Model ▸ Claude Code subscription's rows: T3's Claude models and accounts (provider instances), as the menu lists
    /// them (`crate::relay_catalog`); empty until T3 answered.
    pub relay_models: Vec<ui::RelayModel>,
    pub relay_accounts: Vec<ui::RelayAccount>,
    pub data: std::path::PathBuf,
    /// The user's recent lines, spoken and typed, newest last: what `owner_asked` must quote.
    pub recent_owner: Vec<String>,
}

/// One row of the catalog: the key, its section in the menu, and what `set` takes.
pub struct Key {
    pub key: &'static str,
    pub section: &'static str,
    pub takes: &'static str,
    /// `item` names the microphone, app or task.
    pub item: bool,
}

const fn k(key: &'static str, section: &'static str, takes: &'static str, item: bool) -> Key {
    Key { key, section, takes, item }
}

/// Every setting, in menu order. `agent::mcp_contract::SETTINGS_KEYS` lists the same keys (a test holds them equal).
pub const KEYS: &[Key] = &[
    k("listening", "Tiles", "true or false", false),
    k("seeing", "Tiles", "true or false", false),
    k("speaking", "Tiles", "true or false: your voice on or off", false),
    k("agent", "Tiles", "true or false; false needs owner_asked", false),
    k("engine", "Tiles", "true or false: the built-in inference engine; true also makes its model yours, as in the menu", false),
    k("microphone", "Listening", "a microphone's name, or \"built-in\"", false),
    k("extra_microphone", "Listening", "item: a microphone's name; value true or false", true),
    k("app_audio", "Listening", "item: an app's name; value true (recorded) or false", true),
    k("recording_resolution", "Seeing", "540p, 720p, half, native", false),
    k("recording_fps", "Seeing", "frames per second: 1, 2, 5 or 10", false),
    k("islands_on_streams", "Seeing", "true or false", false),
    k("agent_model", "Agent", "local, an engine model, or an added model's name or id", false),
    k("agent_thinking", "Agent", "the selected model's thinking level", false),
    k("agent_speed", "Agent", "standard or fast: fast asks the Codex subscription or the OpenAI API for the priority tier (an API key bills priority rates); other providers ignore it; only when User asks", false),
    k("agent_context", "Agent", "the selected model's context in tokens (it snaps to the menu's nearest stop)", false),
    k("agent_relay", "Agent", "true or false: Claude Code subscription answers as you (Agent ▸ Model ▸ Claude Code subscription); false goes back to agent_model's row", false),
    k("agent_relay_model", "Agent", "a Claude model T3 lists (id or name); also turns Claude Code subscription on, as its row does", false),
    k("agent_relay_account", "Agent", "automatic, or a Claude account T3 lists (instance id or name); also turns Claude Code subscription on, as its row does", false),
    k("name", "Personalisation", "your name, also your wake word", false),
    k("user_name", "Personalisation", "what you call the user", false),
    k("icon", "Personalisation", "helmet, an SF Symbol name, an absolute PNG/PDF/SVG path, or SVG source", false),
    k("speaking_model", "Personalisation", "drama3, supertonic, phonon, own_engine, elevenlabs", false),
    k("voice", "Personalisation", "a voice of the speaking model", false),
    k("voice_speed", "Personalisation", "percent of natural speed, 80 to 160", false),
    k("wake_sound", "Personalisation", "true or false", false),
    k("ack_delay", "Personalisation", "seconds of quiet after User stops talking before your short spoken acknowledgement and the notch's thinking animation, 0 to 10 (2 or 2.5)", false),
    k("background_model", "Background Tasks", "provider/id, local/<engine model>, or t3:<instance>/<model> (a Claude Code instance in T3, no key: t3:claudeAgent_claude_code_2/claude-haiku-5-5); add @<level> for that model's thinking level (@default: each task's own); with item a task (islands, plan, voice_names, day_timeline, suggestions; suggestions never on t3): that task's own model, or shared for Background Tasks ▸ Model; with item paid_fallback true or false: whether a background task whose backend ran out may end on the Anthropic API key, which costs money (off by default; only when User asks)", false),
    k("background_task", "Background Tasks", "item: islands, plan, voice_names, day_timeline or suggestions; value true or false", true),
    k("show_islands", "Windows", "true or false", false),
    k("launch_at_login", "System", "true or false", false),
];

pub fn key(name: &str) -> Result<&'static Key> {
    KEYS.iter().find(|k| k.key == name).with_context(|| format!("NOT_CHANGED: no setting {name}; call settings with action list"))
}

fn on_off(b: bool) -> Value {
    Value::Bool(b)
}

fn resolution_name(r: ui::ScreenResolution) -> &'static str {
    match r {
        ui::ScreenResolution::P540 => "540p",
        ui::ScreenResolution::P720 => "720p",
        ui::ScreenResolution::P1080 => "1080p",
        ui::ScreenResolution::HalfNative => "half",
        ui::ScreenResolution::Native => "native",
    }
}

/// `background_model`'s item for the fallback's paid step.
fn paid_item(item: &str) -> bool { matches!(fold(item).as_str(), "paidfallback" | "fallbackpaid" | "paid") }

fn task_key(t: ui::BackgroundTask) -> &'static str {
    t.key()
}

fn mic_title(m: &ui::Microphone) -> String {
    if m.built_in { format!("{} (built-in)", m.name) } else { m.name.clone() }
}

impl View {
    fn model_name(&self) -> String {
        match &self.settings.agent_model {
            agent::provider::AgentModel::Local => {
                let id = &self.settings.engine_model;
                // No model loaded yet: the engine itself.
                if id.is_empty() { return "local".into(); }
                let label = self.engine_models.iter().find(|(i, _)| i == id).map_or(id.as_str(), |(_, l)| l.as_str());
                format!("local/{label}")
            }
            agent::provider::AgentModel::Api { provider, id } => {
                let name = self.settings.api_models.iter().find(|m| m.key() == (provider.as_str(), id.as_str())).map_or(id.as_str(), |m| m.name.as_str());
                format!("{provider}/{name}")
            }
            agent::provider::AgentModel::Unchosen => "none".into(),
        }
    }
    fn thinking(&self) -> String {
        match self.settings.selected_api_model() {
            Some(m) if !self.settings.agent_model.is_local() => m.thinking_level(None).to_string(),
            _ => self.settings.engine_thinking_level().to_string(),
        }
    }
    fn context(&self) -> Option<u64> {
        match self.settings.selected_api_model() {
            Some(m) if !self.settings.agent_model.is_local() => Some(m.budget()),
            _ => self.settings.engine_context.get(&self.settings.engine_model).copied(),
        }
    }
    fn primary_microphone(&self) -> String {
        let id = self.microphones.primary.as_ref().or(self.microphones.in_use.as_ref());
        id.and_then(|id| self.microphones.devices.iter().find(|d| &d.id == id)).map_or_else(|| "built-in".into(), mic_title)
    }

    /// The current value of `key` (`item` for per-item settings: None lists them all).
    pub fn get(&self, key: &str, item: Option<&str>) -> Result<Value> {
        let s = &self.settings;
        Ok(match key {
            "listening" => on_off(self.listening),
            "seeing" => on_off(!s.screen_off),
            "speaking" => on_off(s.speak_replies),
            "agent" => on_off(!self.agent_off),
            "engine" => on_off(s.engine_on),
            "microphone" => json!(self.primary_microphone()),
            "extra_microphone" => match item {
                Some(name) => on_off(self.microphones.extras.contains(&find_microphone(&self.microphones, name)?.id)),
                None => json!(self.microphones.devices.iter().filter(|d| self.microphones.extras.contains(&d.id)).map(mic_title).collect::<Vec<_>>()),
            },
            "app_audio" => match item {
                Some(name) => on_off(find_app(&self.apps, name)?.recorded),
                None => json!(self.apps.apps.iter().filter(|a| a.recorded).map(|a| a.name.clone()).collect::<Vec<_>>()),
            },
            "recording_resolution" => json!(resolution_name(s.screen.ui().resolution)),
            "recording_fps" => json!(s.screen.fps),
            "islands_on_streams" => on_off(s.islands_on_streams),
            "agent_model" => json!(self.model_name()),
            "agent_thinking" => json!(self.thinking()),
            "agent_context" => self.context().map_or(json!("the whole window"), |t| json!(t)),
            "agent_relay" => on_off(s.agent_relay),
            // `None` runs T3's first model, as the menu's checked row shows.
            "agent_relay_model" => json!(s.agent_relay_model.clone().or_else(|| self.relay_models.first().map(|m| m.id.clone())).unwrap_or_else(|| "T3's first Claude model".into())),
            "agent_relay_account" => json!(s.agent_relay_account.as_deref().unwrap_or(AUTOMATIC)),
            "name" => json!(s.identity.name),
            "user_name" => json!(s.identity.user_name),
            "icon" => json!(match &s.identity.icon {
                ui::identity::Icon::Helmet => "helmet".to_string(),
                ui::identity::Icon::Symbol { name } => name.clone(),
                ui::identity::Icon::Image { path } => path.display().to_string(),
            }),
            "speaking_model" => json!(speaking_model(&self.voices)),
            "voice" => json!(crate::identity::voice_line(&self.voices)),
            "voice_speed" => json!(s.voice_speed_percent),
            "wake_sound" => on_off(s.wake_sound),
            "ack_delay" => seconds_value(s.ack_delay_ms),
            "agent_speed" => json!(s.agent_speed),
            "background_model" if item.is_some_and(paid_item) => on_off(s.background_fallback_paid),
            "background_model" if item.is_some() => {
                let task = find_task(item.unwrap())?;
                match s.task_models.iter().find(|m| m.task == task.key()) {
                    Some(m) => json!(format!("{}/{}", m.provider, m.id)),
                    None => json!("shared"),
                }
            }
            "background_model" => {
                let m = &s.background_model;
                let level = s.background_efforts.iter().find(|e| e.provider == m.provider && e.id == m.id).map(|e| format!("@{}", e.level)).unwrap_or_default();
                json!(format!("{}/{}{level}", m.provider, m.id))
            }
            "background_task" => match item {
                Some(name) => {
                    let task = task_key(find_task(name)?);
                    on_off(!s.background_off.iter().any(|k| k == task))
                }
                None => json!(ui::BackgroundTask::ALL.iter().filter(|t| !s.background_off.iter().any(|k| k == t.key())).map(|t| t.key()).collect::<Vec<_>>()),
            },
            "show_islands" => on_off(!s.islands_hidden),
            "launch_at_login" => on_off(s.launch_at_login),
            other => bail!("NOT_CHANGED: no setting {other}; call settings with action list"),
        })
    }

    /// The choices `set` takes for `key`, in words (empty when the value says it all).
    pub fn choices(&self, key: &str) -> String {
        match key {
            "microphone" | "extra_microphone" => self.microphones.devices.iter().filter(|d| d.connected).map(mic_title).collect::<Vec<_>>().join(", "),
            "app_audio" => {
                let mut names: Vec<String> = self.apps.apps.iter().map(|a| a.name.clone()).collect();
                names.sort_by_key(|n| n.to_lowercase());
                names.join(", ")
            }
            "agent_model" => {
                let mut names: Vec<String> = self.engine_models.iter().map(|(_, l)| format!("local/{l}")).collect();
                names.extend(self.settings.api_models.iter().filter(|m| m.unsupported().is_none()).map(|m| format!("{}/{}", m.provider, m.name)));
                names.join(", ")
            }
            "agent_relay_model" => self.relay_models.iter().map(|m| format!("{} ({})", m.id, m.name)).collect::<Vec<_>>().join(", "),
            "agent_relay_account" => std::iter::once(AUTOMATIC.to_string()).chain(self.relay_accounts.iter().map(|a| format!("{} ({})", a.id, a.name))).collect::<Vec<_>>().join(", "),
            "agent_thinking" => match self.settings.selected_api_model() {
                Some(m) if !self.settings.agent_model.is_local() => m.thinking_levels(None).join(", "),
                _ => agent::provider::LOCAL_THINKING.join(", "),
            },
            "agent_context" => match self.settings.selected_api_model() {
                Some(m) if !self.settings.agent_model.is_local() => m.context_steps().iter().map(u64::to_string).collect::<Vec<_>>().join(", "),
                _ => String::new(),
            },
            "speaking_model" | "voice" | "icon" => crate::identity::choices(&self.voices),
            "background_task" => ui::BackgroundTask::ALL.map(task_key).join(", "),
            // The T3 instances and their levels as T3 last listed them (Background Tasks ▸ Model's T3 rows).
            "background_model" => crate::background_call::t3::instances().iter().flat_map(|i| i.models.iter().map(move |m| {
                let levels = if m.efforts.is_empty() { String::new() } else { format!(" @{}", m.efforts.join("|")) };
                format!("{}{}/{}{levels}", ui::T3_PROVIDER_PREFIX, i.instance, m.id)
            })).collect::<Vec<_>>().join(", "),
            _ => String::new(),
        }
    }

    /// Every setting with its value, by section: `list`'s answer.
    pub fn list(&self) -> String {
        let mut out = String::new();
        let mut section = "";
        for k in KEYS {
            if k.section != section {
                section = k.section;
                out.push_str(&format!("{}{section}:", if out.is_empty() { "" } else { "\n" }));
            }
            let value = self.get(k.key, None).map_or_else(|e| format!("({e})"), |v| show(&v));
            out.push_str(&format!(" {}={value};", k.key));
        }
        out.push_str("\nUse get with a key for its choices.");
        out
    }
}

/// A value as the agent reads it: strings bare, lists comma-separated.
pub fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) if a.is_empty() => "none".into(),
        Value::Array(a) => a.iter().map(show).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

fn speaking_model(v: &ui::speech_voice::SpeechVoiceState) -> &'static str {
    if v.own {
        return "own_engine";
    }
    if v.api {
        return "elevenlabs";
    }
    match v.local {
        ui::speech_voice::LocalVoice::Phonon => "phonon",
        ui::speech_voice::LocalVoice::Drama3 => "drama3",
        ui::speech_voice::LocalVoice::Supertonic => "supertonic",
    }
}

/// Lower case, letters and digits only: "MacBook Pro Microphone" and "macbook pro microphone" are one name.
fn fold(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}

/// A name the user said, matched as the menu shows names: exact (folded) first, then the one name that contains it.
fn by_name<'a, T>(items: &'a [T], name: &str, title: impl Fn(&T) -> String, what: &str) -> Result<&'a T> {
    let want = fold(name);
    ensure!(!want.is_empty(), "NOT_CHANGED: name a {what}");
    if let Some(found) = items.iter().find(|i| fold(&title(i)) == want) {
        return Ok(found);
    }
    let partial: Vec<&T> = items.iter().filter(|i| fold(&title(i)).contains(&want)).collect();
    match partial.as_slice() {
        [one] => Ok(one),
        [] => bail!("NOT_CHANGED: no {what} named {name}; the menu lists: {}", items.iter().map(&title).collect::<Vec<_>>().join(", ")),
        many => bail!("NOT_CHANGED: \"{name}\" matches several: {}; say which", many.iter().map(|i| title(i)).collect::<Vec<_>>().join(", ")),
    }
}

/// A connected microphone by its name ("built-in" names the built-in one).
pub fn find_microphone<'a>(m: &'a ui::Microphones, name: &str) -> Result<&'a ui::Microphone> {
    let connected: Vec<ui::Microphone> = m.devices.iter().filter(|d| d.connected).cloned().collect();
    let found = if matches!(fold(name).as_str(), "builtin" | "builtinmicrophone" | "default") {
        connected.iter().find(|d| d.built_in).context("NOT_CHANGED: no built-in microphone is connected")?
    } else {
        by_name(&connected, name, |d| d.name.clone(), "connected microphone")?
    };
    m.devices.iter().find(|d| d.id == found.id).context("NOT_CHANGED: the microphone went away")
}

/// An app of the App Audio submenu by its name ("Ego" finds "Ego Browser").
pub fn find_app<'a>(a: &'a ui::AppAudio, name: &str) -> Result<&'a ui::App> {
    ensure!(!a.apps.is_empty(), "NOT_CHANGED: App Audio lists no apps yet; an app joins the list once it plays sound");
    by_name(&a.apps, name, |x| x.name.clone(), "app in App Audio")
}

fn find_task(name: &str) -> Result<ui::BackgroundTask> {
    let want = fold(name);
    ui::BackgroundTask::ALL
        .into_iter()
        .find(|t| fold(t.key()) == want || fold(t.title()) == want)
        .with_context(|| format!("NOT_CHANGED: no background task {name}; one of {}", ui::BackgroundTask::ALL.map(task_key).join(", ")))
}

fn boolean(value: &Value, key: &str) -> Result<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::String(s) if matches!(s.to_lowercase().as_str(), "on" | "true") => Ok(true),
        Value::String(s) if matches!(s.to_lowercase().as_str(), "off" | "false") => Ok(false),
        _ => bail!("NOT_CHANGED: {key} takes true or false"),
    }
}

fn number(value: &Value, key: &str) -> Result<u64> {
    match value {
        Value::Number(n) => n.as_u64().with_context(|| format!("NOT_CHANGED: {key} takes a whole number")),
        Value::String(s) => s.trim().trim_end_matches('%').trim().parse().with_context(|| format!("NOT_CHANGED: {key} takes a whole number")),
        _ => bail!("NOT_CHANGED: {key} takes a whole number"),
    }
}

/// Milliseconds as the user says them: whole seconds bare (2), else with their fraction (2.5).
fn seconds_value(ms: u32) -> Value {
    if ms % 1000 == 0 { json!(ms / 1000) } else { json!(f64::from(ms) / 1000.) }
}

/// Seconds as the user says them (2, 2.5, "2.5 s", "1500 ms") in milliseconds, 0 up to `MAX_ACK_DELAY_MS`.
fn seconds_ms(value: &Value, key: &str) -> Result<u32> {
    let max = crate::settings::MAX_ACK_DELAY_MS;
    let wrong = || format!("NOT_CHANGED: {key} is seconds, 0 to {}", max / 1000);
    let (number, unit) = match value {
        Value::Number(n) => (n.as_f64(), 1000.),
        Value::String(s) => {
            let t = s.trim().to_lowercase();
            let (t, unit) = match t.strip_suffix("ms") {
                Some(t) => (t.to_string(), 1.),
                None => (t.trim_end_matches(|c: char| c.is_alphabetic()).to_string(), 1000.),
            };
            (t.trim().replace(',', ".").parse::<f64>().ok(), unit)
        }
        _ => (None, 1000.),
    };
    let ms = number.filter(|n| n.is_finite() && *n >= 0.).map(|n| (n * unit).round()).with_context(wrong)?;
    ensure!(ms <= f64::from(max), "{} ({} is more; it takes seconds, not milliseconds)", wrong(), show(value));
    Ok(ms as u32)
}

fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value.as_str().map(str::trim).filter(|s| !s.is_empty()).with_context(|| format!("NOT_CHANGED: {key} takes text"))
}

/// The user's own words for a change that turns the agent off (`owner_asked`): at least three words, quoted from one of his
/// recent lines (`recent`, spoken or typed; letters and digits compared, case and punctuation ignored).
pub fn agent_off_check(owner_asked: Option<&str>, recent: &[String]) -> Result<String> {
    let words = owner_asked.map(str::trim).filter(|w| w.split_whitespace().filter(|x| x.chars().any(char::is_alphabetic)).count() >= 3);
    let words = words.context(
        "NOT_CHANGED: you never switch yourself off unless User asked; pass owner_asked with his exact words if he did",
    )?;
    let quoted = fold(words);
    ensure!(
        recent.iter().any(|line| fold(line).contains(&quoted)),
        "NOT_CHANGED: owner_asked must quote User's own recent words, as the transcript has them; these are not in his recent lines"
    );
    Ok(words.to_string())
}

/// What a `set` sends: the menu's own actions, in order, and the agent-off words when the change needs them.
#[derive(Debug, PartialEq)]
pub struct Plan {
    pub actions: Vec<UiAction>,
    /// The user's words that allowed the agent to go off (logged).
    pub agent_off_by: Option<String>,
}

impl Plan {
    fn one(a: UiAction) -> Self {
        Self { actions: vec![a], agent_off_by: None }
    }
}

/// The menu actions that set `key` to `value`: Err (prefixed NOT_CHANGED) when the menu would not offer it.
pub fn plan(view: &View, key_name: &str, item: Option<&str>, value: &Value, owner_asked: Option<&str>) -> Result<Plan> {
    let k = key(key_name)?;
    ensure!(!k.item || item.is_some(), "NOT_CHANGED: {key_name} needs item: {}", k.takes);
    let s = &view.settings;
    Ok(match key_name {
        "listening" => Plan::one(UiAction::SetListening(boolean(value, key_name)?)),
        "seeing" => Plan::one(UiAction::SetSeeing(boolean(value, key_name)?)),
        "speaking" => Plan::one(UiAction::SetSpeakReplies(boolean(value, key_name)?)),
        "agent" => {
            let on = boolean(value, key_name)?;
            let by = if on { None } else { Some(agent_off_check(owner_asked, &view.recent_owner)?) };
            Plan { actions: vec![UiAction::SetDryRun(!on)], agent_off_by: by }
        }
        "engine" => {
            let on = boolean(value, key_name)?;
            // The engine off under a running local agent is the agent off.
            let by = if !on && s.agent_model.is_local() && !view.agent_off { Some(agent_off_check(owner_asked, &view.recent_owner)?) } else { None };
            Plan { actions: vec![UiAction::SetEngine(on)], agent_off_by: by }
        }
        "microphone" => Plan::one(UiAction::SelectMicrophone { id: find_microphone(&view.microphones, text(value, key_name)?)?.id.clone() }),
        "extra_microphone" => {
            let on = boolean(value, key_name)?;
            // Off works on a saved extra that is not connected now, as its "Not Connected" row in the menu does.
            let mic = if on { find_microphone(&view.microphones, item.unwrap())? } else { by_name(&view.microphones.devices, item.unwrap(), |d| d.name.clone(), "microphone")? };
            let main = view.microphones.primary.as_ref().or(view.microphones.in_use.as_ref());
            ensure!(!on || main != Some(&mic.id), "NOT_CHANGED: {} is the main microphone already", mic.name);
            Plan::one(UiAction::SetMicrophoneOn { id: mic.id.clone(), on })
        }
        "app_audio" => {
            let app = find_app(&view.apps, item.unwrap())?;
            Plan::one(UiAction::SetAppRecorded { id: app.id.clone(), on: boolean(value, key_name)? })
        }
        "recording_resolution" => {
            let want = fold(text(value, key_name)?);
            let resolution = ui::ScreenResolution::MENU
                .into_iter()
                .find(|r| fold(resolution_name(*r)) == want || fold(&r.title()) == want || (want == "halfnative" && *r == ui::ScreenResolution::HalfNative))
                .context("NOT_CHANGED: recording_resolution is 540p, 720p, half or native")?;
            Plan::one(UiAction::SetScreenRecording(ui::ScreenRecording { resolution, fps: s.screen.fps }))
        }
        "recording_fps" => {
            let fps = number(value, key_name)?;
            ensure!(ui::SCREEN_FPS.iter().any(|f| u64::from(*f) == fps), "NOT_CHANGED: recording_fps is 1, 2, 5 or 10");
            Plan::one(UiAction::SetScreenRecording(ui::ScreenRecording { resolution: s.screen.ui().resolution, fps: fps as u32 }))
        }
        "islands_on_streams" => Plan::one(UiAction::SetIslandsOnStreams(boolean(value, key_name)?)),
        "agent_model" => Plan::one(agent_model(view, text(value, key_name)?)?),
        "agent_relay" => match boolean(value, key_name)? {
            true => Plan::one(UiAction::SelectAgentRelay),
            // Off already: nothing to click (the model's row would start or load it again).
            false if !s.agent_relay => Plan { actions: vec![], agent_off_by: None },
            false => Plan::one(relay_off(view)?),
        },
        "agent_relay_model" => {
            let v = text(value, key_name)?;
            ensure!(!view.relay_models.is_empty(), "NOT_CHANGED: T3 has not listed its Claude models yet (is T3 Code running?)");
            let m = view.relay_models.iter().find(|m| m.id == v).map_or_else(|| by_name(&view.relay_models, v, |m| m.name.clone(), "Claude model T3 lists"), Ok)?;
            Plan::one(UiAction::SelectRelayModel(m.id.clone()))
        }
        "agent_relay_account" => {
            let v = text(value, key_name)?;
            if fold(v) == fold(AUTOMATIC) { return Ok(Plan::one(UiAction::SelectRelayAccount(None))); }
            ensure!(!view.relay_accounts.is_empty(), "NOT_CHANGED: T3 has not listed its Claude accounts yet (is T3 Code running?); automatic works now");
            let a = view.relay_accounts.iter().find(|a| a.id == v).map_or_else(|| by_name(&view.relay_accounts, v, |a| a.name.clone(), "Claude account T3 lists"), Ok)?;
            Plan::one(UiAction::SelectRelayAccount(Some(a.id.clone())))
        }
        "agent_thinking" => {
            let level = text(value, key_name)?.to_lowercase();
            match s.selected_api_model() {
                Some(m) if !s.agent_model.is_local() => {
                    ensure!(m.thinking_levels(None).contains(&level.as_str()), "NOT_CHANGED: {} thinks at {}", m.name, m.thinking_levels(None).join(", "));
                    Plan::one(UiAction::SetApiModelThinking { provider: m.provider.clone(), id: m.id.clone(), level })
                }
                _ => {
                    ensure!(agent::provider::LOCAL_THINKING.contains(&level.as_str()), "NOT_CHANGED: the local model thinks at {}", agent::provider::LOCAL_THINKING.join(", "));
                    Plan::one(UiAction::SetEngineThinking(level))
                }
            }
        }
        "agent_context" => {
            let tokens = number(value, key_name)?;
            match s.selected_api_model() {
                Some(m) if !s.agent_model.is_local() => {
                    let steps = m.context_steps();
                    let (lo, hi) = (steps.first().copied().unwrap_or(0), steps.last().copied().unwrap_or(0));
                    ensure!((lo..=hi).contains(&tokens), "NOT_CHANGED: {} takes {lo} to {hi} tokens", m.name);
                    Plan::one(UiAction::SetApiModelContext { provider: m.provider.clone(), id: m.id.clone(), tokens })
                }
                _ => {
                    // The menu's stops start at 128K (or the model's whole window, when smaller); the budget snaps to them.
                    ensure!((128_000..=1_048_576).contains(&tokens), "NOT_CHANGED: the local model takes 128000 tokens up to its window");
                    Plan::one(UiAction::SetEngineContext(tokens))
                }
            }
        }
        "name" | "user_name" | "icon" => {
            use ui::identity::IdentityAction as A;
            let v = text(value, key_name)?;
            let action = match key_name {
                "name" => A::SetName(v.to_string()),
                "user_name" => A::SetUserName(v.to_string()),
                _ if v.starts_with('<') => A::SetIcon(ui::identity::Icon::Image { path: crate::identity::save_svg(&view.data, v)? }),
                _ => A::SetIcon(crate::identity::icon_arg(v)?),
            };
            let mut next = s.identity.clone();
            next.apply(&action).map_err(|e| anyhow::anyhow!("NOT_CHANGED: {e}"))?;
            Plan::one(UiAction::Identity(action))
        }
        "speaking_model" => {
            let args = json!({"speaking_model": text(value, key_name)?.to_lowercase()});
            Plan::one(UiAction::SpeechVoice(crate::identity::speech_voice_action(&args, &view.voices, &view.presets)?.context("NOT_CHANGED: no model")?))
        }
        "voice" => {
            let model = speaking_model(&view.voices);
            ensure!(!matches!(model, "drama3" | "supertonic"), "NOT_CHANGED: {model} has one voice; set speaking_model first (phonon, own_engine or elevenlabs)");
            let args = json!({"speaking_model": speaking_model(&view.voices), "voice": text(value, key_name)?});
            Plan::one(UiAction::SpeechVoice(crate::identity::speech_voice_action(&args, &view.voices, &view.presets)?.context("NOT_CHANGED: no voice")?))
        }
        "voice_speed" => {
            let p = number(value, key_name)?;
            let (lo, hi) = (speak::stretch::MIN_PERCENT as u64, speak::stretch::MAX_PERCENT as u64);
            ensure!((lo..=hi).contains(&p), "NOT_CHANGED: voice_speed is {lo} to {hi} percent");
            Plan::one(UiAction::SetVoiceSpeed(p as u16))
        }
        "wake_sound" => Plan::one(UiAction::SetWakeSound(boolean(value, key_name)?)),
        "ack_delay" => Plan::one(UiAction::SetAckDelay(seconds_ms(value, key_name)?)),
        "agent_speed" => {
            let v = text(value, key_name)?.to_ascii_lowercase();
            ensure!(matches!(v.as_str(), "standard" | "fast"), "NOT_CHANGED: agent_speed is standard or fast");
            Plan::one(UiAction::SetAgentSpeed(v))
        }
        // A task's own model ("islands model"), or "shared": back on Background Tasks ▸ Model.
        // The fallback's paid step (the Anthropic API key): only when the user says so, as for any money choice.
        "background_model" if item.is_some_and(paid_item) => Plan::one(UiAction::SetBackgroundFallbackPaid(boolean(value, key_name)?)),
        "background_model" if item.is_some() => {
            let task = find_task(item.unwrap())?;
            ensure!(crate::background_model::reads_own_model(task), "NOT_CHANGED: {} runs on Background Tasks ▸ Model only", task.title());
            let v = text(value, key_name)?;
            if matches!(v.trim().to_ascii_lowercase().as_str(), "shared" | "default" | "") {
                return Ok(Plan::one(UiAction::SetTaskModel { task, provider: String::new(), id: String::new() }));
            }
            let (provider, id) = v.split_once('/').context("NOT_CHANGED: a task's model is provider/id, local/<engine model>, t3:<instance>/<model>, or shared")?;
            let (provider, id) = if provider == "local" { (ui::LOCAL_ENGINE, engine_model_id(view, id)?) } else { (provider, id.to_string()) };
            let m = crate::background_model::BackgroundModel::from_ui(provider, &id).with_context(|| format!("NOT_CHANGED: no provider {provider}"))?;
            ensure!(crate::background_model::task_accepts(task, &m), "NOT_CHANGED: {} searches with tools, which a T3 thread cannot run: pick an API or local model", task.title());
            Plan::one(UiAction::SetTaskModel { task, provider: m.provider, id: m.id })
        }
        "background_model" => {
            let v = text(value, key_name)?;
            let (v, level) = v.rsplit_once('@').map_or((v, None), |(m, l)| (m, Some(l.trim())));
            let (provider, id) = v.split_once('/').context("NOT_CHANGED: background_model is provider/id, local/<engine model> or t3:<instance>/<model>")?;
            let (provider, id) = if provider == "local" { (ui::LOCAL_ENGINE, engine_model_id(view, id)?) } else { (provider, id.to_string()) };
            let m = crate::background_model::BackgroundModel::from_ui(provider, &id).with_context(|| format!("NOT_CHANGED: no provider {provider}"))?;
            let mut actions = vec![UiAction::SetBackgroundModel { provider: m.provider.clone(), id: m.id.clone() }];
            if let Some(level) = level {
                let level = if level.eq_ignore_ascii_case("default") { "" } else { level };
                // A T3 model's own levels, when T3 listed them.
                let listed = ui::t3_instance(&m.provider).map(|i| crate::background_call::t3::efforts(i, &m.id)).unwrap_or_default();
                ensure!(level.is_empty() || listed.is_empty() || listed.iter().any(|l| l == level), "NOT_CHANGED: {} takes {}", m.id, listed.join(", "));
                let e = crate::background_model::BackgroundEffort::from_ui(&m.provider, &m.id, level).context("NOT_CHANGED: a thinking level is one word")?;
                actions.push(UiAction::SetBackgroundEffort { provider: e.provider, id: e.id, level: e.level });
            }
            Plan { actions, agent_off_by: None }
        }
        "background_task" => Plan::one(UiAction::SetBackgroundTask { task: find_task(item.unwrap())?, on: boolean(value, key_name)? }),
        "show_islands" => Plan::one(UiAction::SetIslandsShown(boolean(value, key_name)?)),
        "launch_at_login" => Plan::one(UiAction::SetLaunchAtLogin(boolean(value, key_name)?)),
        other => bail!("NOT_CHANGED: no setting {other}"),
    })
}

/// The older `personalise` call (kept for callers that learned it): its arguments as one plan, refused whole when one
/// value is wrong. `wake_words` is ignored: the wake word is the name.
pub fn plan_personalise(view: &View, args: &Value) -> Result<Plan> {
    let mut actions: Vec<UiAction> =
        crate::identity::identity_actions(&view.settings.identity, args, &view.data)?.into_iter().map(UiAction::Identity).collect();
    if let Some(a) = crate::identity::speech_voice_action(args, &view.voices, &view.presets)? {
        actions.push(UiAction::SpeechVoice(a));
    }
    Ok(Plan { actions, agent_off_by: None })
}

/// `personalise`'s one line: the identity and the voice.
pub fn personal_line(view: &View) -> String {
    crate::identity::describe(&view.settings.identity, &crate::identity::voice_line(&view.voices))
}

fn engine_model_id(view: &View, name: &str) -> Result<String> {
    by_name(&view.engine_models, name, |(id, label)| if fold(id) == fold(name) { id.clone() } else { label.clone() }, "engine model").map(|(id, _)| id.clone())
}

/// `agent_relay_account`'s word for `None`: the menu's Automatic row.
const AUTOMATIC: &str = "automatic";

/// Claude Code subscription off: the row of the model the agent runs without it, as a click on that row ends the relay.
fn relay_off(view: &View) -> Result<UiAction> {
    let s = &view.settings;
    Ok(match &s.agent_model {
        // This Mac's row: the agent on whatever the engine has loaded, the engine untouched (the menu's own row).
        agent::provider::AgentModel::Local => UiAction::SelectLocalAgent,
        agent::provider::AgentModel::Api { provider, id } => UiAction::SelectApiModel { provider: provider.clone(), id: id.clone() },
        agent::provider::AgentModel::Unchosen => bail!("NOT_CHANGED: no model to go back to; set agent_model, which ends Claude Code subscription"),
    })
}

/// `agent_model`'s value as the Agent ▸ Model row that picks it.
fn agent_model(view: &View, v: &str) -> Result<UiAction> {
    let s = &view.settings;
    let (scope, name) = v.split_once('/').map_or((None, v), |(a, b)| (Some(a), b));
    // "local": the agent on whatever the engine has loaded, as Agent ▸ Model's This Mac row picks it; "local/<model>"
    // also loads that model (the user named it).
    if fold(v) == "local" {
        return Ok(UiAction::SelectLocalAgent);
    }
    if scope == Some("local") {
        return Ok(UiAction::SelectEngineModel(engine_model_id(view, name)?));
    }
    let added: Vec<&agent::provider::ApiModel> =
        s.api_models.iter().filter(|m| scope.is_none_or(|p| p == m.provider)).filter(|m| m.unsupported().is_none()).collect();
    let found = added.iter().find(|m| m.id == name).copied().map_or_else(|| by_name(&added, name, |m| m.name.clone(), "added model").copied(), Ok);
    match found {
        Ok(m) => {
            let p = m.provider_info();
            ensure!(p.is_none_or(agent::provider::has_credential), "NOT_CHANGED: {} is not connected; User connects it in the menu", m.provider);
            Ok(UiAction::SelectApiModel { provider: m.provider.clone(), id: m.id.clone() })
        }
        Err(e) if scope.is_none() => engine_model_id(view, name).map(UiAction::SelectEngineModel).map_err(|_| e),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ui::identity::IdentityAction;

    fn mic(id: &str, name: &str, built_in: bool) -> ui::Microphone {
        ui::Microphone { id: id.into(), name: name.into(), connected: true, built_in }
    }
    fn app(id: &str, name: &str, recorded: bool) -> ui::App {
        ui::App { id: id.into(), name: name.into(), icon_path: None, playing: false, recorded }
    }

    /// A fake Mac: three microphones (the built-in one in use), two apps, the bundled engine models, Phonon S.
    pub(crate) fn view(data: &std::path::Path) -> View {
        View {
            settings: Settings::default(),
            agent_off: false,
            listening: true,
            microphones: ui::Microphones {
                devices: vec![mic("BuiltInMicrophoneDevice", "MacBook Pro Microphone", true), mic("usb-1", "Shure MV7", false), mic("airpods-2", "User's AirPods Pro", false)],
                primary: None,
                in_use: Some("BuiltInMicrophoneDevice".into()),
                extras: vec![],
            },
            apps: ui::AppAudio { apps: vec![app("com.ego.browser", "Ego Browser", false), app("com.hnc.Discord", "Discord", true)] },
            voices: ui::speech_voice::demo_speech_voice(false, ui::speech_voice::LocalVoice::Phonon, true),
            presets: vec![],
            engine_models: vec![("qwen3.8-27b".into(), "Qwen 3.8 27B".into())],
            relay_models: vec![ui::RelayModel { id: "claude-opus-5-5".into(), name: "Claude Opus 5.5".into() }, ui::RelayModel { id: "claude-sonnet-5-5".into(), name: "Claude Sonnet 5.5".into() }],
            relay_accounts: vec![ui::RelayAccount { id: "claudeAgent".into(), name: "Claude".into() }, ui::RelayAccount { id: "claudeAgent_claude_code_2".into(), name: "Claude (work)".into() }],
            data: data.into(),
            recent_owner: vec!["User: the build is green".into(), "Okay, turn the agent off for tonight.".into()],
        }
    }

    fn set(v: &View, key: &str, item: Option<&str>, value: Value) -> Result<Vec<UiAction>> {
        plan(v, key, item, &value, None).map(|p| p.actions)
    }

    #[test]
    fn the_catalog_is_the_contracts() {
        let ours: Vec<&str> = KEYS.iter().map(|k| k.key).collect();
        assert_eq!(ours, agent::mcp_contract::SETTINGS_KEYS);
    }

    #[test]
    fn every_key_reads_and_lists() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        for k in KEYS {
            v.get(k.key, None).unwrap_or_else(|e| panic!("{}: {e}", k.key));
        }
        let list = v.list();
        assert!(list.starts_with("Tiles: listening=true; seeing=true;"), "{list}");
        assert!(list.contains("\nPersonalisation: name=Heimdall; user_name=User; icon=helmet;"), "{list}");
        assert!(list.contains("microphone=MacBook Pro Microphone (built-in);") && list.contains("app_audio=Discord;"), "{list}");
        assert!(list.contains(" wake_sound=true; ack_delay=0;"), "{list}");
        assert!(v.get("nonsense", None).is_err());
    }

    /// ("2.5 seconds is way too long, 2 seconds"): `ack_delay` in seconds as he says them, stored in
    /// milliseconds; out of range (or milliseconds passed as seconds) sends nothing.
    #[test]
    fn ack_delay_takes_seconds() {
        let t = tempfile::tempdir().unwrap();
        let mut v = view(t.path());
        assert_eq!(v.get("ack_delay", None).unwrap(), json!(0), "no added wait by default");
        for (said, ms) in [(json!(2), 2_000), (json!(2.5), 2_500), (json!("1.5 s"), 1_500), (json!("3 seconds"), 3_000), (json!("1500 ms"), 1_500), (json!(0), 0), (json!(10), 10_000)] {
            assert_eq!(set(&v, "ack_delay", None, said.clone()).unwrap(), [UiAction::SetAckDelay(ms)], "{said}");
        }
        for bad in [json!(10.5), json!(2000), json!(-1), json!("soon"), json!(true)] {
            assert!(set(&v, "ack_delay", None, bad.clone()).unwrap_err().to_string().starts_with("NOT_CHANGED: ack_delay is seconds"), "{bad}");
        }
        v.settings.ack_delay_ms = 2_500;
        assert_eq!(v.get("ack_delay", None).unwrap(), json!(2.5));
    }

    /// Standard or Fast. Standard by default; only the two words pass.
    #[test]
    fn agent_speed_is_standard_or_fast() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(v.get("agent_speed", None).unwrap(), json!("standard"));
        assert_eq!(set(&v, "agent_speed", None, json!("Fast")).unwrap(), [UiAction::SetAgentSpeed("fast".into())]);
        assert_eq!(set(&v, "agent_speed", None, json!("standard")).unwrap(), [UiAction::SetAgentSpeed("standard".into())]);
        for bad in [json!("priority"), json!(true), json!("turbo")] {
            assert!(set(&v, "agent_speed", None, bad.clone()).unwrap_err().to_string().starts_with("NOT_CHANGED: agent_speed"), "{bad}");
        }
    }

    /// Tiles: each maps to the tile's own action; a word for a boolean is read, anything else refused.
    #[test]
    fn tiles() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "listening", None, json!(false)).unwrap(), [UiAction::SetListening(false)]);
        assert_eq!(set(&v, "seeing", None, json!("off")).unwrap(), [UiAction::SetSeeing(false)]);
        assert_eq!(set(&v, "speaking", None, json!(true)).unwrap(), [UiAction::SetSpeakReplies(true)]);
        assert_eq!(set(&v, "agent", None, json!(true)).unwrap(), [UiAction::SetDryRun(false)]);
        assert_eq!(set(&v, "engine", None, json!(true)).unwrap(), [UiAction::SetEngine(true)]);
        assert!(set(&v, "speaking", None, json!("loud")).is_err());
        assert!(set(&v, "seeing", None, json!(3)).is_err());
    }

    /// The agent never turns itself off on its own say-so: refused without the user's words, allowed and reported with them.
    #[test]
    fn the_agent_off_needs_owners_words() {
        let t = tempfile::tempdir().unwrap();
        let mut v = view(t.path());
        let e = plan(&v, "agent", None, &json!(false), None).unwrap_err().to_string();
        assert!(e.starts_with("NOT_CHANGED: you never switch yourself off"), "{e}");
        assert!(plan(&v, "agent", None, &json!("off"), Some("  ")).is_err());
        assert!(plan(&v, "agent", None, &json!(false), Some("yes")).is_err(), "too short to be his request");
        let e = plan(&v, "agent", None, &json!(false), Some("User asked me to stop")).unwrap_err().to_string();
        assert!(e.contains("not in his recent lines"), "words he never said are refused: {e}");
        let p = plan(&v, "agent", None, &json!(false), Some("turn the agent off for tonight")).unwrap();
        assert_eq!(p, Plan { actions: vec![UiAction::SetDryRun(true)], agent_off_by: Some("turn the agent off for tonight".into()) });
        // The engine under a local agent is the agent too; under an API model it is not.
        v.settings.agent_model = agent::provider::AgentModel::Local;
        assert!(plan(&v, "engine", None, &json!(false), None).is_err());
        assert!(plan(&v, "engine", None, &json!(false), Some("TURN the agent off")).unwrap().agent_off_by.is_some(), "case and punctuation ignored");
        assert!(plan(&v, "engine", None, &json!(false), Some("stop the engine")).is_err());
        v.settings.agent_model = agent::provider::AgentModel::default();
        assert_eq!(plan(&v, "engine", None, &json!(false), None).unwrap().actions, [UiAction::SetEngine(false)]);
    }

    /// Microphones by the names the menu shows, never by id: exact, folded, a unique part, "built-in".
    #[test]
    fn microphones_by_name() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "microphone", None, json!("Shure MV7")).unwrap(), [UiAction::SelectMicrophone { id: "usb-1".into() }]);
        assert_eq!(set(&v, "microphone", None, json!("shure")).unwrap(), [UiAction::SelectMicrophone { id: "usb-1".into() }]);
        assert_eq!(set(&v, "microphone", None, json!("users airpods pro")).unwrap(), [UiAction::SelectMicrophone { id: "airpods-2".into() }]);
        assert_eq!(set(&v, "microphone", None, json!("built-in")).unwrap(), [UiAction::SelectMicrophone { id: "BuiltInMicrophoneDevice".into() }]);
        assert_eq!(set(&v, "extra_microphone", Some("AirPods"), json!(true)).unwrap(), [UiAction::SetMicrophoneOn { id: "airpods-2".into(), on: true }]);
        let e = set(&v, "microphone", None, json!("Blue Yeti")).unwrap_err().to_string();
        assert!(e.contains("no connected microphone named Blue Yeti") && e.contains("Shure MV7"), "{e}");
        let e = set(&v, "microphone", None, json!("o")).unwrap_err().to_string();
        assert!(e.contains("matches several"), "{e}");
        assert!(set(&v, "extra_microphone", None, json!(true)).is_err(), "names the mic in item");
        assert!(set(&v, "extra_microphone", Some("MacBook Pro Microphone"), json!(true)).is_err(), "the main mic is not an extra");
        let mut gone = v.clone();
        gone.microphones.devices[1].connected = false;
        assert!(set(&gone, "microphone", None, json!("Shure MV7")).is_err(), "a saved mic that is not connected cannot be chosen");
        assert_eq!(set(&gone, "extra_microphone", Some("Shure"), json!(false)).unwrap(), [UiAction::SetMicrophoneOn { id: "usb-1".into(), on: false }], "but a saved extra can be turned off");
        let mut chosen = v.clone();
        chosen.microphones.primary = Some("usb-1".into());
        assert!(set(&chosen, "extra_microphone", Some("Shure"), json!(true)).is_err(), "the chosen main mic is not an extra");
        assert_eq!(v.get("microphone", None).unwrap(), json!("MacBook Pro Microphone (built-in)"));
        assert_eq!(v.get("extra_microphone", Some("shure")).unwrap(), json!(false));
    }

    /// "Enable app audio from Ego browser": the App Audio row by the app's name.
    #[test]
    fn app_audio_by_app_name() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "app_audio", Some("Ego browser"), json!(true)).unwrap(), [UiAction::SetAppRecorded { id: "com.ego.browser".into(), on: true }]);
        assert_eq!(set(&v, "app_audio", Some("ego"), json!(true)).unwrap(), [UiAction::SetAppRecorded { id: "com.ego.browser".into(), on: true }]);
        assert_eq!(set(&v, "app_audio", Some("Discord"), json!(false)).unwrap(), [UiAction::SetAppRecorded { id: "com.hnc.Discord".into(), on: false }]);
        assert!(set(&v, "app_audio", Some("Safari"), json!(true)).unwrap_err().to_string().contains("Ego Browser, Discord"));
        assert_eq!((v.get("app_audio", Some("Discord")).unwrap(), v.get("app_audio", None).unwrap()), (json!(true), json!(["Discord"])));
        let empty = View { apps: ui::AppAudio::default(), ..v };
        assert!(set(&empty, "app_audio", Some("Ego"), json!(true)).unwrap_err().to_string().contains("once it plays sound"));
    }

    #[test]
    fn seeing() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "recording_resolution", None, json!("720p")).unwrap(), [UiAction::SetScreenRecording(ui::ScreenRecording { resolution: ui::ScreenResolution::P720, fps: 5 })]);
        assert_eq!(set(&v, "recording_fps", None, json!(10)).unwrap(), [UiAction::SetScreenRecording(ui::ScreenRecording { resolution: ui::ScreenResolution::HalfNative, fps: 10 })]);
        assert!(set(&v, "recording_resolution", None, json!("8k")).is_err());
        assert!(set(&v, "recording_fps", None, json!(0)).is_err() && set(&v, "recording_fps", None, json!(30)).is_err(), "only the menu's rates");
        assert_eq!(set(&v, "islands_on_streams", None, json!(true)).unwrap(), [UiAction::SetIslandsOnStreams(true)]);
        assert_eq!(v.get("recording_resolution", None).unwrap(), json!("half"));
    }

    /// The agent's model among the added ones (by name or id) or the engine's; thinking and context for the selected one.
    #[test]
    fn agent_model_thinking_and_context() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "agent_model", None, json!("local")).unwrap(), [UiAction::SelectLocalAgent]);
        assert_eq!(set(&v, "agent_model", None, json!("local/Qwen 3.8 27B")).unwrap(), [UiAction::SelectEngineModel("qwen3.8-27b".into())]);
        assert!(set(&v, "agent_model", None, json!("gpt-9-ultra")).is_err());
        let level = v.settings.selected_api_model().map(|m| m.thinking_levels(None)).unwrap_or_default();
        if let (Some(m), Some(l)) = (v.settings.selected_api_model(), level.last()) {
            assert_eq!(set(&v, "agent_thinking", None, json!(l)).unwrap(), [UiAction::SetApiModelThinking { provider: m.provider.clone(), id: m.id.clone(), level: l.to_string() }]);
        }
        assert!(set(&v, "agent_thinking", None, json!("extreme")).is_err());
        let mut local = v.clone();
        local.settings.agent_model = agent::provider::AgentModel::Local;
        assert_eq!(set(&local, "agent_thinking", None, json!("high")).unwrap(), [UiAction::SetEngineThinking("high".into())]);
        assert!(set(&local, "agent_thinking", None, json!("max")).is_err(), "the local model has no max");
        assert_eq!(set(&local, "agent_context", None, json!(128_000)).unwrap(), [UiAction::SetEngineContext(128_000)]);
        assert!(set(&local, "agent_context", None, json!(8192)).is_err(), "below the menu's first stop");
        assert_eq!(local.get("agent_model", None).unwrap(), json!("local"), "no model loaded yet: the engine itself");
        local.settings.engine_model = "qwen3.8-27b".into();
        assert_eq!(local.get("agent_model", None).unwrap(), json!("local/Qwen 3.8 27B"), "the engine model by its menu label");
    }

    /// Claude Code subscription: on and off, its model and its account, each as the menu's own row; only what
    /// T3 lists (the menu's rows) is offered or taken.
    #[test]
    fn claude_via_t3_its_model_and_account() {
        let t = tempfile::tempdir().unwrap();
        let mut v = view(t.path());
        assert_eq!((v.get("agent_relay", None).unwrap(), v.get("agent_relay_model", None).unwrap(), v.get("agent_relay_account", None).unwrap()),
            (json!(false), json!("claude-opus-5-5"), json!("automatic")), "off, on T3's first model, automatic");
        assert_eq!(set(&v, "agent_relay", None, json!(true)).unwrap(), [UiAction::SelectAgentRelay]);
        assert_eq!(set(&v, "agent_relay_model", None, json!("claude-sonnet-5-5")).unwrap(), [UiAction::SelectRelayModel("claude-sonnet-5-5".into())]);
        assert_eq!(set(&v, "agent_relay_model", None, json!("sonnet")).unwrap(), [UiAction::SelectRelayModel("claude-sonnet-5-5".into())], "by the menu's name");
        assert_eq!(set(&v, "agent_relay_account", None, json!("claudeAgent_claude_code_2")).unwrap(), [UiAction::SelectRelayAccount(Some("claudeAgent_claude_code_2".into()))]);
        assert_eq!(set(&v, "agent_relay_account", None, json!("Claude (work)")).unwrap(), [UiAction::SelectRelayAccount(Some("claudeAgent_claude_code_2".into()))]);
        assert_eq!(set(&v, "agent_relay_account", None, json!("Automatic")).unwrap(), [UiAction::SelectRelayAccount(None)]);
        let e = set(&v, "agent_relay_model", None, json!("claude-haiku-9")).unwrap_err().to_string();
        assert!(e.starts_with("NOT_CHANGED: no Claude model T3 lists named claude-haiku-9") && e.contains("Claude Opus 5.5"), "{e}");
        let e = set(&v, "agent_relay_account", None, json!("claudeAgent_3")).unwrap_err().to_string();
        assert!(e.starts_with("NOT_CHANGED: no Claude account T3 lists") && e.contains("Claude (work)"), "{e}");
        assert!(v.choices("agent_relay_account").starts_with("automatic, claudeAgent (Claude), claudeAgent_claude_code_2"), "{}", v.choices("agent_relay_account"));
        assert!(v.choices("agent_relay_model").contains("claude-sonnet-5-5 (Claude Sonnet 5.5)"));
        assert!(set(&v, "agent_relay", None, json!(false)).unwrap().is_empty(), "off already: nothing sent");
        // Off: the row of the model it goes back to.
        v.settings.agent_relay = true;
        v.settings.agent_model = agent::provider::AgentModel::Local;
        assert_eq!(set(&v, "agent_relay", None, json!(false)).unwrap(), [UiAction::SelectLocalAgent]);
        v.settings.agent_model = agent::provider::AgentModel::Api { provider: "anthropic".into(), id: "claude-opus-5-5".into() };
        assert_eq!(set(&v, "agent_relay", None, json!(false)).unwrap(), [UiAction::SelectApiModel { provider: "anthropic".into(), id: "claude-opus-5-5".into() }]);
        v.settings.agent_model = agent::provider::AgentModel::Unchosen;
        assert!(set(&v, "agent_relay", None, json!(false)).is_err());
        // T3 not answered yet: nothing to offer but Automatic.
        let none = View { relay_models: vec![], relay_accounts: vec![], ..v.clone() };
        assert!(set(&none, "agent_relay_model", None, json!("claude-opus-5-5")).unwrap_err().to_string().contains("has not listed"));
        assert!(set(&none, "agent_relay_account", None, json!("claudeAgent")).unwrap_err().to_string().contains("has not listed"));
        assert_eq!(set(&none, "agent_relay_account", None, json!("automatic")).unwrap(), [UiAction::SelectRelayAccount(None)]);
        v.settings.agent_relay_account = Some("claudeAgent_claude_code_2".into());
        assert_eq!(v.get("agent_relay_account", None).unwrap(), json!("claudeAgent_claude_code_2"));
    }

    /// Personalisation: the name (the wake word), the user's name, the mark, the speaking model and voice, the speed
    /// and the wake sound, each as the menu's own action, invalid values refused before anything is sent.
    #[test]
    fn personalisation() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "name", None, json!("Ivy")).unwrap(), [UiAction::Identity(IdentityAction::SetName("Ivy".into()))]);
        assert_eq!(set(&v, "user_name", None, json!("Evelyn")).unwrap(), [UiAction::Identity(IdentityAction::SetUserName("Evelyn".into()))]);
        assert!(set(&v, "name", None, json!("Ivy!")).unwrap_err().to_string().starts_with("NOT_CHANGED: Only letters"));
        assert!(set(&v, "name", None, json!("Okay")).is_err());
        assert_eq!(set(&v, "icon", None, json!("helmet")).unwrap(), [UiAction::Identity(IdentityAction::SetIcon(ui::identity::Icon::Helmet))]);
        let svg = r#"<svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="6"/></svg>"#;
        let drawn = set(&v, "icon", None, json!(svg)).unwrap();
        let [UiAction::Identity(IdentityAction::SetIcon(ui::identity::Icon::Image { path }))] = drawn.as_slice() else { panic!("{drawn:?}") };
        assert!(path.starts_with(t.path().join("identity")));
        assert_eq!(set(&v, "voice", None, json!("bryony")).unwrap(), [UiAction::SpeechVoice(ui::speech_voice::SpeechVoiceAction::SetPreset("Bryony".into()))]);
        assert!(set(&v, "voice", None, json!("Nobody")).is_err());
        assert_eq!(set(&v, "speaking_model", None, json!("Supertonic")).unwrap(), [UiAction::SpeechVoice(ui::speech_voice::SpeechVoiceAction::SetLocal(ui::speech_voice::LocalVoice::Supertonic))]);
        assert!(set(&v, "speaking_model", None, json!("elevenlabs")).is_err(), "ElevenLabs needs a voice");
        let mut drama = v.clone();
        drama.voices.local = ui::speech_voice::LocalVoice::Drama3;
        assert!(set(&drama, "voice", None, json!("Bryony")).is_err(), "Drama 3 has one voice: the voice is not quietly ignored");
        assert_eq!(set(&v, "voice_speed", None, json!(120)).unwrap(), [UiAction::SetVoiceSpeed(120)]);
        assert_eq!(set(&v, "voice_speed", None, json!("110%")).unwrap(), [UiAction::SetVoiceSpeed(110)]);
        assert!(set(&v, "voice_speed", None, json!(200)).is_err() && set(&v, "voice_speed", None, json!(50)).is_err());
        assert_eq!(set(&v, "wake_sound", None, json!(false)).unwrap(), [UiAction::SetWakeSound(false)]);
        assert_eq!(v.get("voice", None).unwrap(), json!(format!("phonon · {}", v.voices.preset)));
    }

    #[test]
    fn background_windows_and_login() {
        let t = tempfile::tempdir().unwrap();
        let v = view(t.path());
        assert_eq!(set(&v, "background_model", None, json!("anthropic/claude-sonnet-5-5")).unwrap(), [UiAction::SetBackgroundModel { provider: "anthropic".into(), id: "claude-sonnet-5-5".into() }]);
        assert_eq!(set(&v, "background_model", None, json!("local/qwen 3.8 27b")).unwrap(), [UiAction::SetBackgroundModel { provider: ui::LOCAL_ENGINE.into(), id: "qwen3.8-27b".into() }]);
        assert!(set(&v, "background_model", None, json!("nowhere/x")).is_err() && set(&v, "background_model", None, json!("sonnet")).is_err());
        assert_eq!(set(&v, "background_task", Some("Voice Names"), json!(false)).unwrap(), [UiAction::SetBackgroundTask { task: ui::BackgroundTask::VoiceNames, on: false }]);
        assert_eq!(set(&v, "background_task", Some("day_timeline"), json!(true)).unwrap(), [UiAction::SetBackgroundTask { task: ui::BackgroundTask::DayTimeline, on: true }]);
        assert!(set(&v, "background_task", Some("mining"), json!(true)).is_err() && set(&v, "background_task", None, json!(true)).is_err());
        assert_eq!(set(&v, "show_islands", None, json!(false)).unwrap(), [UiAction::SetIslandsShown(false)]);
        assert_eq!(set(&v, "launch_at_login", None, json!(true)).unwrap(), [UiAction::SetLaunchAtLogin(true)]);
        assert!(set(&v, "launch_at_login", None, json!("sometimes")).is_err());
        assert_eq!(v.get("background_task", None).unwrap(), json!(["islands", "plan", "goals", "voice_names", "day_timeline", "suggestions"]));
        assert_eq!(set(&v, "background_task", Some("plan"), json!(false)).unwrap(), [UiAction::SetBackgroundTask { task: ui::BackgroundTask::Plan, on: false }]);
    }

    /// A T3 instance and a model's thinking level, as the menu sends them; a Codex instance never in the personal edition.
    #[test]
    fn background_model_on_t3_with_a_level() {
        let t = tempfile::tempdir().unwrap();
        let mut v = view(t.path());
        let t3 = "t3:claudeAgent_claude_code_2";
        assert_eq!(set(&v, "background_model", None, json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5")).unwrap(),
            [UiAction::SetBackgroundModel { provider: t3.into(), id: "claude-haiku-5-5".into() }]);
        assert_eq!(set(&v, "background_model", None, json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5@low")).unwrap(), [
            UiAction::SetBackgroundModel { provider: t3.into(), id: "claude-haiku-5-5".into() },
            UiAction::SetBackgroundEffort { provider: t3.into(), id: "claude-haiku-5-5".into(), level: "low".into() }]);
        assert_eq!(set(&v, "background_model", None, json!("anthropic/claude-haiku-5-5@default")).unwrap()[1],
            UiAction::SetBackgroundEffort { provider: "anthropic".into(), id: "claude-haiku-5-5".into(), level: String::new() });
        if !crate::edition::PUBLIC { assert!(set(&v, "background_model", None, json!("t3:codex/gpt-6.1-sol")).is_err(), "never Codex via T3"); }
        // With T3's list known, a level it does not list is refused, and the choices name the levels.
        crate::background_call::t3::publish_catalog(&serde_json::from_str(include_str!("background_t3_capabilities.json")).unwrap());
        assert!(set(&v, "background_model", None, json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5@ultracode")).is_err());
        assert!(v.choices("background_model").contains("t3:claudeAgent_claude_code_2/claude-haiku-5-5 @low|medium|high|xhigh|max|ultrathink"));
        v.settings.background_model = crate::background_model::BackgroundModel { provider: t3.into(), id: "claude-haiku-5-5".into() };
        v.settings.background_efforts = vec![crate::background_model::BackgroundEffort { provider: t3.into(), id: "claude-haiku-5-5".into(), level: "low".into() }];
        assert_eq!(v.get("background_model", None).unwrap(), json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5@low"));
    }

    /// The fallback's paid step: off by default, set only as a yes or no.
    #[test]
    fn the_paid_fallback_is_a_switch() {
        let t = tempfile::tempdir().unwrap();
        let mut v = view(t.path());
        assert_eq!(v.get("background_model", Some("paid_fallback")).unwrap(), json!(false));
        assert_eq!(set(&v, "background_model", Some("paid fallback"), json!(true)).unwrap(), [UiAction::SetBackgroundFallbackPaid(true)]);
        assert!(set(&v, "background_model", Some("paid_fallback"), json!("maybe")).is_err());
        v.settings.background_fallback_paid = true;
        assert_eq!(v.get("background_model", Some("paid_fallback")).unwrap(), json!(true));
    }

    /// A task's own model (the islands model): set, read, and back to the shared one.
    #[test]
    fn a_tasks_own_model() {
        let t = tempfile::tempdir().unwrap();
        let mut v = view(t.path());
        assert_eq!(set(&v, "background_model", Some("islands"), json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5")).unwrap(),
            [UiAction::SetTaskModel { task: ui::BackgroundTask::Islands, provider: "t3:claudeAgent_claude_code_2".into(), id: "claude-haiku-5-5".into() }]);
        assert_eq!(set(&v, "background_model", Some("Voice Names"), json!("anthropic/claude-sonnet-5-5")).unwrap(),
            [UiAction::SetTaskModel { task: ui::BackgroundTask::VoiceNames, provider: "anthropic".into(), id: "claude-sonnet-5-5".into() }]);
        assert_eq!(set(&v, "background_model", Some("islands"), json!("shared")).unwrap(),
            [UiAction::SetTaskModel { task: ui::BackgroundTask::Islands, provider: String::new(), id: String::new() }]);
        assert!(set(&v, "background_model", Some("mining"), json!("anthropic/x")).is_err() && set(&v, "background_model", Some("islands"), json!("nowhere/x")).is_err());
        // Suggestions searches with tools: never on T3.
        assert!(set(&v, "background_model", Some("suggestions"), json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5")).unwrap_err().to_string().contains("T3"));
        assert_eq!(set(&v, "background_model", Some("suggestions"), json!("openai/gpt-6-luna")).unwrap(),
            [UiAction::SetTaskModel { task: ui::BackgroundTask::Suggestions, provider: "openai".into(), id: "gpt-6-luna".into() }]);
        v.settings.task_models = crate::background_model::default_task_models();
        let islands = if crate::edition::PUBLIC { json!("shared") } else { json!("t3:claudeAgent_claude_code_2/claude-haiku-5-5") };
        assert_eq!(v.get("background_model", Some("islands")).unwrap(), islands);
        assert_eq!(v.get("background_model", Some("plan")).unwrap(), json!("shared"));
    }
}
