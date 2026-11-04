//! Speaking ▸ the voice (night 1022 menu): Local (Drama 3, Supertonic or Phonon S with its presets) or API
//! (ElevenLabs with its voice picker). The app sends the whole state (`UiUpdate::SpeechVoice`); the menu answers with
//! `UiAction::SpeechVoice`. ElevenLabs' key goes through the providers' key panel as provider `ELEVENLABS`
//! (`UiAction::ConnectProviderKey`, answered by `UiUpdate::ProviderConnected`).

/// The key panel's provider id for ElevenLabs.
pub const ELEVENLABS: &str = "elevenlabs";
pub const ELEVENLABS_NAME: &str = "ElevenLabs";
/// Where ElevenLabs keys are made (opened beside the key panel).
pub const ELEVENLABS_KEYS_URL: &str = "https://elevenlabs.io/app/settings/api-keys";
/// [voices] The key panel's provider id for the own speech engine (custom-URL mode, key optional).
pub const OWN_VOICE: &str = "own-voice";
pub const OWN_VOICE_NAME: &str = "Own Voice Engine";
/// [voices] Phonon S's suggested voices ("2–4 suggested voices, mostly male … firm, calm and
/// British-leaning"), chosen from the 37 presets' catalogue descriptions (not by ear): Edwin ("efficient and firm,
/// sincere", UK), Spencer ("slow, patient delivery", UK), Preston ("trim, focused … steady, low-set core", US) and
/// Bryony ("full, rich … softly relaxed", UK, the one woman). The menu shows only these, plus a saved other preset.
pub const SUGGESTED_PRESETS: [&str; 4] = ["Edwin", "Spencer", "Preston", "Bryony"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalVoice {
    /// Drama 3 (Jarvis): the big model, the user's voice. The personal edition's default.
    Drama3,
    /// Supertonic 3: the small model.
    Supertonic,
    /// Phonon S (31M), with presets.
    Phonon,
}
/// What the menu shows before the app's first state: the edition's default voice. The public edition has Phonon S
/// only, so its menu never says Drama 3, even for a moment (VM finding P-02, 28 Oct).
impl Default for LocalVoice {
    fn default() -> Self {
        if cfg!(feature = "public") { LocalVoice::Phonon } else { LocalVoice::Drama3 }
    }
}
impl LocalVoice {
    pub const ALL: [LocalVoice; 3] = [LocalVoice::Drama3, LocalVoice::Supertonic, LocalVoice::Phonon];
    pub fn title(self) -> &'static str {
        match self {
            LocalVoice::Drama3 => "Drama 3",
            LocalVoice::Supertonic => "Supertonic",
            LocalVoice::Phonon => "Phonon S",
        }
    }
}

/// One Phonon S preset as the submenu shows it: grouped, named, Gradium's description as the tooltip.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoicePreset {
    pub group: String,
    pub name: String,
    pub desc: String,
}

/// One ElevenLabs voice from the account's `/v1/voices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiVoice {
    pub id: String,
    pub name: String,
    pub desc: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ElevenLabsState {
    pub connected: bool,
    /// The account's voices (empty until listed).
    pub voices: Vec<ApiVoice>,
    pub loading: bool,
    /// Why the list or the key failed, in ElevenLabs' words.
    pub problem: Option<String>,
    /// The chosen voice: id and name (kept while the list is loading or failed).
    pub selected: Option<(String, String)>,
}

/// [voices] Own Engine: a local OpenAI-compatible speech endpoint (`POST <base>/audio/speech`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnVoiceState {
    /// The saved base URL ("http://localhost:8880/v1"); `None`: not connected.
    pub endpoint: Option<String>,
    /// The endpoint's voices (its list, else OpenAI's standard names).
    pub voices: Vec<ApiVoice>,
    pub loading: bool,
    pub problem: Option<String>,
    /// The chosen voice: id and name.
    pub selected: Option<(String, String)>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeechVoiceState {
    /// [voices] Own Engine is the chosen source; wins over `api`.
    pub own: bool,
    pub own_engine: OwnVoiceState,
    /// API chosen (else Local).
    pub api: bool,
    pub local: LocalVoice,
    pub preset: String,
    pub presets: Vec<VoicePreset>,
    /// Setup's voice grid: every everyday Phonon S preset (no Characters), Edwin first (`app::speech_voice::everyday`).
    /// The menu keeps `presets`.
    pub everyday: Vec<VoicePreset>,
    pub elevenlabs: ElevenLabsState,
    /// [voices] Drama 3's model is on this Mac (the user's private voice): its row shows; elsewhere only
    /// Phonon S is built in.
    pub drama3_installed: bool,
}
impl SpeechVoiceState {
    /// The presets' groups in first-appearance order with their presets (the submenu's sections).
    pub fn groups(&self) -> Vec<(&str, Vec<&VoicePreset>)> {
        let mut groups: Vec<(&str, Vec<&VoicePreset>)> = vec![];
        for p in &self.presets {
            match groups.iter_mut().find(|(g, _)| *g == p.group) {
                Some((_, list)) => list.push(p),
                None => groups.push((&p.group, vec![p])),
            }
        }
        groups
    }
    /// The ElevenLabs row's detail: the chosen voice, else what it needs.
    pub fn elevenlabs_detail(&self) -> Option<String> {
        let e = &self.elevenlabs;
        if !e.connected {
            return Some("Not Connected".into());
        }
        e.selected.as_ref().map(|(_, name)| name.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpeechVoiceAction {
    /// Local (false) or API (true).
    SetApi(bool),
    SetLocal(LocalVoice),
    /// A Phonon S preset by name (also chooses Phonon S, Local).
    SetPreset(String),
    /// An ElevenLabs voice (also chooses API).
    SetElevenLabsVoice { id: String, name: String },
    /// List the account's voices again.
    RefreshElevenLabs,
    DisconnectElevenLabs,
    /// [voices] An Own Engine voice (also chooses Own Engine).
    SetOwnVoice { id: String, name: String },
    DisconnectOwnEngine,
    /// List the own engine's voices again.
    RefreshOwnEngine,
}

/// The everyday presets as the app orders them (voices.json, 4 Nov): Edwin, then US women, UK women, US men, UK men.
const EVERYDAY_DEMO: [&str; 25] = [
    "Edwin", "Zoey", "Juniper", "Everly", "Harper", "Hadley", "Delia", "Maeve", "Freya", "Pippa", "Elodie-Rose", "Imogen", "Lottie",
    "Bryony", "Easton", "Micah", "Trevor", "Russell", "Preston", "Miles", "Alfie", "Hugh", "Spencer", "Toby", "Teddy",
];

/// A demo state for renders: the user's default (Local, Drama 3) with Phonon S's suggested voices and a connected ElevenLabs.
pub fn demo_speech_voice(api: bool, local: LocalVoice, connected: bool) -> SpeechVoiceState {
    let preset = |group: &str, name: &str, desc: &str| VoicePreset { group: group.into(), name: name.into(), desc: desc.into() };
    let voice = |id: &str, name: &str| ApiVoice { id: id.into(), name: name.into(), desc: String::new() };
    SpeechVoiceState {
        api,
        local,
        // The user's Mac has Drama 3; the public build does not (renders of other voices show the public menu).
        drama3_installed: local == LocalVoice::Drama3,
        preset: "Edwin".into(),
        // [voices] The suggested voices as the app publishes them: one flat list, no groups.
        presets: vec![
            preset("", "Edwin", "Efficient and firm, sincere in attention, with tidy articulation that holds up at phone bandwidth."),
            preset("", "Spencer", "Light and airy, sitting a touch higher, with crisp consonants and a slow, patient delivery."),
            preset("", "Preston", "Trim, focused delivery with clean edges on every consonant and a steady, low-set core."),
            preset("", "Bryony", "Full, rich mid-range tones, softly relaxed and spoken close to the mic."),
        ],
        // Setup's grid: the 25 everyday presets in the app's order (names only; the app sends Gradium's descriptions).
        everyday: EVERYDAY_DEMO.iter().map(|n| preset("", n, "")).collect(),
        elevenlabs: ElevenLabsState {
            connected,
            voices: if connected { vec![voice("21m00Tcm4TlvDq8ikWAM", "Rachel"), voice("pNInz6obpgDQGcFmaJgB", "Adam"), voice("JBFqnCBsd6RMkjVDRZzb", "George")] } else { vec![] },
            loading: false,
            problem: None,
            selected: connected.then(|| ("JBFqnCBsd6RMkjVDRZzb".into(), "George".into())),
        },
        own: false,
        // [voices] A connected own speech engine with three voices (Kokoro-style ids).
        own_engine: OwnVoiceState {
            endpoint: Some("http://localhost:8880/v1".into()),
            voices: vec![voice("af_heart", "Heart"), voice("af_bella", "Bella"), voice("am_adam", "Adam")],
            loading: false,
            problem: None,
            selected: Some(("af_heart".into(), "Heart".into())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn groups_keep_file_order_and_details_say_what_is_missing() {
        let s = demo_speech_voice(false, LocalVoice::Phonon, false);
        let groups: Vec<&str> = s.groups().iter().map(|(g, _)| *g).collect();
        // The suggested voices are one flat list.
        assert_eq!(groups, [""]);
        assert_eq!(s.presets.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["Edwin", "Spencer", "Preston", "Bryony"]);
        assert_eq!(s.elevenlabs_detail().as_deref(), Some("Not Connected"));
        let c = demo_speech_voice(true, LocalVoice::Drama3, true);
        assert_eq!(c.elevenlabs_detail().as_deref(), Some("George"));
    }
}

#[cfg(test)]
mod default_tests {
    #[test]
    fn the_menu_starts_on_the_editions_default_voice() {
        let want = if cfg!(feature = "public") { super::LocalVoice::Phonon } else { super::LocalVoice::Drama3 };
        assert_eq!(super::SpeechVoiceState::default().local, want);
    }
}
