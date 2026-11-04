//! Editions (docs/editions.md): what each build speaks with and how it listens. Every test runs in both builds:
//! `cargo test -p app edition`, and the same with `--features app/public` (scripts/check.sh, `public-edition`).
use crate::speech_voice::{Route, VoiceChoice};
use ui::speech_voice::{LocalVoice, SpeechVoiceAction as A};

/// Personal starts on Jarvis; in public the default (Drama 3) and a saved Drama 3 or Supertonic choice speak with
/// Phonon S, since neither model is in that bundle.
#[test]
fn edition_sets_the_default_voice_and_where_a_local_choice_goes() {
    let public = super::PUBLIC;
    let phonon = Route::Phonon(speak::phonon::DEFAULT_PRESET.into());
    let mut c = VoiceChoice::default();
    assert_eq!(c.route(), if public { phonon.clone() } else { Route::Jarvis });
    assert_eq!(crate::settings::Settings::default().speech_voice.route(), c.route());
    c.apply(&A::SetLocal(LocalVoice::Supertonic));
    assert_eq!(c.route(), if public { phonon.clone() } else { Route::Supertonic });
    c.apply(&A::SetLocal(LocalVoice::Drama3));
    assert_eq!(c.route(), if public { phonon.clone() } else { Route::Jarvis });
    c.apply(&A::SetPreset("Edwin".into()));
    assert_eq!(c.route(), Route::Phonon("Edwin".into()), "a Phonon S choice is the same in both");
    // The user's saved file (Drama 3) opened by a public build speaks, with Phonon S.
    let saved: VoiceChoice = serde_json::from_value(serde_json::json!({"source":"local","local":"drama3","phonon_preset":"Edwin"})).unwrap();
    assert_eq!(saved.route(), if public { Route::Phonon("Edwin".into()) } else { Route::Jarvis });
}

/// The public edition's Smart Turn is the stock model, which has no Metal data: it starts on the CPU instead of
/// falling back with a "Listening using fallback" warning and a GPU retry every 30 s. The user's stays on Metal.
#[test]
fn edition_turn_backend_matches_the_bundled_smart_turn() {
    let s = crate::settings::Settings::default();
    assert_eq!(s.turn_backend, if super::PUBLIC { "cpu" } else { "metal" });
    assert!(s.turn_backend_migrated, "a fresh file is never migrated back to Metal");
}

/// The public edition starts blank: no agent model, no pre-added models, no monthly credit, no background model.
/// The personal edition keeps the owner's defaults exactly.
#[test]
fn edition_blank_start_is_public_only() {
    use agent::provider::{AgentModel, ApiModel};
    let s = crate::settings::Settings::default();
    if super::PUBLIC {
        assert_eq!(s.agent_model, AgentModel::Unchosen);
        assert!(s.api_models.is_empty() && s.anthropic_credit_usd == 0 && s.background_model.is_none());
    } else {
        assert_eq!(s.agent_model, AgentModel::Api { provider: "opencode-go".into(), id: "claude-haiku-5-5".into() });
        assert_eq!(s.api_models.len(), 4);
        assert_eq!(s.api_models[0], ApiModel::haiku());
        assert_eq!(s.anthropic_credit_usd, 100);
        assert_eq!((s.background_model.provider.as_str(), s.background_model.id.as_str()), ("anthropic", "claude-haiku-5-5"));
    }
    // A fresh public profile stays blank through a save and load (no Anthropic rows come back, nothing is chosen).
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("settings.json");
    s.save(&path).unwrap();
    let back = crate::settings::Settings::load_and_migrate(&path).unwrap();
    assert_eq!((back.agent_model, back.api_models.len()), (s.agent_model.clone(), s.api_models.len()));
}

/// Every Keychain service the app uses has its own name in the public edition, so the public app never reads the
/// personal app's keys; the personal names never change.
#[test]
fn edition_keychain_names_differ_between_editions() {
    let mut names: Vec<&str> = agent::provider::PROVIDERS.iter().map(|p| p.keychain_service).collect();
    names.extend([crate::own_voice::KEYCHAIN_SERVICE, crate::speech_voice::KEYCHAIN_SERVICE, t3::client::KEYCHAIN_SERVICE]);
    if super::PUBLIC {
        assert!(names.iter().all(|n| n.starts_with("Heimdall Alpha ") || *n == "com.tobynoskillson.heimdall.t3"), "{names:?}");
    } else {
        assert_eq!(names, ["Heimdall API provider: ChatGPT", "Heimdall API provider: OpenCode Go", "Heimdall API provider: Anthropic",
            "Heimdall API provider: OpenAI-Compatible", "Heimdall API provider: Anthropic-Compatible", "Heimdall voice provider: Own Engine",
            "Heimdall API provider: ElevenLabs", "dev.heimdall.t3"]);
    }
}
