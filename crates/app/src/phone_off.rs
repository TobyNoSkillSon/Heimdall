//! The phone (its bridge, its endpoint, the phone's microphone, presence and reports) is not part of
//! this source release; it comes in a later one. Like Friends (friends.rs), the shared code keeps its calls and they
//! land here as no-ops with the same names: no listener opens, no bridge starts, nothing is ever sent to a phone, and
//! no line is ever the phone's (docs/editions.md).

pub mod remote {
    use anyhow::Result;
    use serde_json::{json, Value};


    /// Nobody listens: no phone is ever connected.
    pub struct Hub;
    impl Hub {
        pub fn listeners(&self) -> usize { 0 }
    }
    pub static HUB: Hub = Hub;

    #[derive(Debug, Clone, Copy)]
    pub enum MacVia { Typed, Drop, OwnerVoice }

    pub fn chat_capped(day: Value, rows: Vec<Value>, older: u64) -> Value { json!({"day": day, "rows": rows, "older": older}) }
    pub fn with_image_data(args: Value, _load: impl Fn(&str) -> Option<Vec<u8>>) -> Value { args }
    pub fn publish(_kind: &str, _args: Value) {}
    pub fn answered(_request_id: &str, _answers: &[ui::agent_notice::QuestionAnswer]) {}
    pub fn said_on_phone(_text: &str) -> Option<&str> { None }
    pub fn phone_speaker() -> String { agent::identity::DEFAULT_USER.into() }
    pub fn phone_speaking() -> bool { false }
    pub fn phone_owns_output() -> bool { false }
    pub fn phone_turn_id() -> Option<String> { None }
    pub fn phone_turn_failed() {}
    pub fn phone_holds_mic() -> bool { false }
    pub fn phone_mic_wake(_observation: Option<&str>) -> String { String::new() }
    pub fn talk_over_stop() {}
    pub fn with_speech_turn<R>(_turn: Option<String>, f: impl FnOnce() -> R) -> R { f() }
    pub fn delivery_failed(_id: &str) {}
    pub fn follow_phone_delivery(_fate: impl Fn(&str) -> Option<agent::DeliveryFate>) {}
    pub fn mac_turn_started_via(_via: MacVia) {}
    pub fn mac_turn_delivery(_id: &str) {}
    pub fn mac_turn_failed() {}
    pub fn spoken_wake_delivered(_observation: Option<&str>, _outcome: &agent::WakeOutcome) {}
    pub fn relay_input_run(_input: &str, _run: Option<&str>) {}
    pub fn relay_reply_begins(_input: Option<&str>, _run: Option<&str>) {}
    pub fn relay_reply_said(_input: Option<&str>, _run: Option<&str>) {}
    pub fn agent_event(_event: &agent::protocol::Event, _settled: bool) {}
    pub fn speak_on_phone(_text: &str, _language: &str) -> bool { false }
    pub fn reply_speech_on_phone(_text: &str, _language: &str) -> bool { false }
    pub fn audio_on_phone(_text: &str, _language: &str, _wav: Vec<u8>) -> bool { false }
    pub fn speak_args(text: &str, language: &str, _wav: Option<&[u8]>) -> Value { json!({"text": text, "language": language}) }
    pub fn wav_bytes(_samples: &[f32], _sample_rate: u32) -> Result<Vec<u8>> { Ok(Vec::new()) }
    pub fn lend_asr(_worker: Option<std::sync::Arc<listen::ListenWorker>>) {}
    pub fn asr_ready() -> bool { false }
    pub fn expire_asks() {}
}

pub mod remote_jobs {
    pub fn card_update(_update: &ui::UiUpdate) {}
}

pub mod remote_listen {
    pub fn live_pass(_wanted: bool, _models: bool, _setup: bool, _ready: bool) {}
}

pub mod remote_settings {
    use crate::speech_voice::Route;
    /// A voice's title as the Mac's menu names it (the voice speaking now, `voice_now`).
    pub fn route_title(route: &Route, state: &ui::speech_voice::SpeechVoiceState) -> String {
        use ui::speech_voice::LocalVoice;
        match route {
            Route::Jarvis => LocalVoice::Drama3.title().into(),
            Route::Supertonic => LocalVoice::Supertonic.title().into(),
            Route::Phonon(p) => format!("{} · {p}", LocalVoice::Phonon.title()),
            Route::ElevenLabs(id) => format!("{} · {}", ui::speech_voice::ELEVENLABS_NAME,
                state.elevenlabs.selected.as_ref().filter(|(i, _)| i == id).map_or(id.as_str(), |(_, n)| n.as_str())),
            Route::Own(id) => format!("{} · {}", ui::speech_voice::OWN_VOICE_NAME,
                state.own_engine.selected.as_ref().filter(|(i, _)| i == id).map_or(id.as_str(), |(_, n)| n.as_str())),
        }
    }
    pub fn from_phone() -> bool { false }
    pub fn tick<V>(_key: impl FnOnce() -> String, _view: impl FnOnce() -> V, _voice_now: impl FnOnce() -> String) {}
}

pub mod remote_presence {
    /// The configured user's name as a notice says it (`name_from`).
    pub fn user_name() -> String { name_from(&agent::identity::current().user) }
    /// A configured user name as the notice says it: "the user" for none or the public edition's unchosen `User`.
    pub fn name_from(user: &str) -> String {
        let user = user.trim();
        if user.is_empty() || user == agent::identity::PUBLIC_USER { "the user".into() } else { user.to_owned() }
    }
}

pub mod phone_mic {
    /// No source is ever the phone's.
    pub fn is_phone_source(_id: &str) -> bool { false }
    pub fn set_input(_input: Option<capture::PushInput>) {}
    pub fn take_press_line() -> bool { false }
}

pub mod phone_listen {
    pub fn heard_on_phone(_text: &str) -> bool { false }
}
