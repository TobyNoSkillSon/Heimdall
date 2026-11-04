//! Personalise: who the assistant is — its name (which is also its wake word), the user's name
//! and the mark — saved in settings.json as `identity` (`ui::identity::Identity`). The agent changes it with the
//! `settings` tool (`crate::settings_tool`; `personalise` is its older name) through the menu's own actions
//! (docs/personalise.md).
//!
//! `publish` hands the names to every part that reads them: the UI (menu, islands, the mark), the decider and the wake
//! spotter (`decide::names`), the agent's prompts (`agent::identity`, filled at the next Pi start) and the speech
//! recognizer's name bias (`listen::vocabulary`, at the next model load). Everything applies live; only the agent's
//! prompt needs a Pi reload, which the pipeline schedules after the running turn (the `reload_me` path), never a
//! Heimdall restart.
use std::path::{Path, PathBuf};

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;
use ui::identity::{Icon, Identity};

/// Every reader of the names, from now on. The UI gets its copy through `UiUpdate::Identity`.
/// Not `ui::identity::set`: the UI sets its own copy on `UiUpdate::Identity` and redraws then (set here first, the
/// update would find nothing changed and leave the mark and menu stale).
pub fn publish(identity: &Identity) {
    decide::names::set(decide::names::Names::new(&identity.name, &identity.user_name));
    agent::identity::set(agent::identity::Names { assistant: identity.name.clone(), user: identity.user_name.clone() });
    listen::vocabulary::set_extra_words(asr_words(identity));
    screen::set_name_words(identity.wake_name());
}

/// The words the recognizer leans toward: the name as written ("Ivy"). Heimdall's own name is already in the bundled
/// list, so the default adds nothing.
pub fn asr_words(identity: &Identity) -> Vec<String> {
    let mut words = vec![identity.name.clone()];
    words.retain(|w| !w.eq_ignore_ascii_case(ui::identity::DEFAULT_NAME));
    words
}

/// What changed between two identities, for the apply path.
pub struct Change {
    /// The agent's prompt says the names: reload Pi after its turn.
    pub names: bool,
    /// The recognizer's bias list changed: reload the speech models.
    pub asr: bool,
}
pub fn change(old: &Identity, new: &Identity) -> Change {
    Change {
        names: (&old.name, &old.user_name) != (&new.name, &new.user_name),
        asr: asr_words(old) != asr_words(new),
    }
}

/// An icon argument: "helmet", an SF Symbol name, or an absolute PNG, PDF or SVG path (which must exist).
pub fn icon_arg(icon: &str) -> Result<Icon> {
    if icon == "helmet" {
        return Ok(Icon::Helmet);
    }
    if icon.contains('/') {
        let path = PathBuf::from(icon);
        ensure!(path.is_file(), "NOT_CHANGED: no file at {icon}");
        return Ok(Icon::Image { path });
    }
    #[cfg(target_os = "macos")]
    ensure!(ui::sf_symbol_exists(icon), "NOT_CHANGED: there is no SF Symbol named {icon}");
    Ok(Icon::Symbol { name: icon.to_string() })
}

/// An SVG the agent drew, kept in `<data>/identity/` under a name from its content (the same drawing, the same file).
pub fn save_svg(data: &Path, svg: &str) -> Result<PathBuf> {
    let head = svg.trim_start();
    ensure!(head.starts_with("<svg") || head.starts_with("<?xml"), "NOT_CHANGED: icon_svg is not SVG source");
    ensure!(svg.contains("</svg>") || svg.trim_end().ends_with("/>"), "NOT_CHANGED: icon_svg is not a whole SVG document");
    ensure!(plain_shapes(svg), "NOT_CHANGED: icon_svg must be plain shapes: no scripts, images, entities, foreign objects or outside links");
    let dir = data.join("identity");
    crate::settings::private_dir(&dir)?;
    // FNV-1a: a stable name for the same drawing.
    let hash = svg.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
    let path = dir.join(format!("mark-{hash:016x}.svg"));
    crate::settings::private_write(&path, svg.as_bytes()).context("save the icon")?;
    // One drawing is kept: the one in use (earlier drawings go; the setting points at this one now).
    for old in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
        let p = old.path();
        let ours = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("mark-") && n.ends_with(".svg"));
        if ours && p != path { let _ = std::fs::remove_file(p); }
    }
    Ok(path)
}

/// An SVG with nothing but shapes: no scripts, DTDs or entities (entity expansion), raster images, foreign objects,
/// event handlers or links other than to its own fragments (`href="#…"`).
fn plain_shapes(svg: &str) -> bool {
    let lower = svg.to_ascii_lowercase();
    if ["<script", "<!doctype", "<!entity", "<image", "<foreignobject", "<iframe", "javascript:", "@import", "url(http", "url(file", "url(data"]
        .iter().any(|bad| lower.contains(bad)) {
        return false;
    }
    if lower.split(|c: char| c.is_whitespace()).any(|attr| attr.starts_with("on") && attr.contains('=')) {
        return false;
    }
    // Every href (plain or xlink:) points inside the document.
    lower.match_indices("href").all(|(at, _)| {
        let rest = lower[at + 4..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else { return true };
        let rest = rest.trim_start().trim_start_matches(['"', '\'']);
        rest.starts_with('#')
    })
}

/// One line for the agent: the current identity and voice.
pub fn describe(identity: &Identity, voice: &str) -> String {
    let icon = match &identity.icon {
        Icon::Helmet => "helmet".to_string(),
        Icon::Symbol { name } => name.clone(),
        Icon::Image { path } => path.display().to_string(),
    };
    format!(
        "Name {} · user {} · icon {} · speaking {}",
        identity.name,
        identity.user_name,
        icon,
        voice
    )
}

/// The older `personalise` arguments as the menu's identity actions, checked together on a copy of `identity`: one
/// refused value refuses them all, so nothing is sent (the voice is the caller's: `speech_voice_action`). Err says
/// what is wrong, prefixed for the agent.
pub fn identity_actions(identity: &Identity, args: &Value, data: &Path) -> Result<Vec<ui::identity::IdentityAction>> {
    use ui::identity::IdentityAction as A;
    let mut next = identity.clone();
    let mut actions = vec![];
    let mut step = |action: A| -> Result<()> {
        next.apply(&action).map_err(|e| anyhow::anyhow!("NOT_CHANGED: {e}"))?;
        actions.push(action);
        Ok(())
    };
    if let Some(name) = args["name"].as_str() {
        step(A::SetName(name.into()))?;
    }
    if let Some(user) = args["user_name"].as_str() {
        step(A::SetUserName(user.into()))?;
    }
    // `wake_words` from an older caller is ignored: the wake word is the name.
    if let Some(icon) = args["icon"].as_str() {
        step(A::SetIcon(icon_arg(icon)?))?;
    }
    if let Some(svg) = args["icon_svg"].as_str() {
        step(A::SetIcon(Icon::Image { path: save_svg(data, svg)? }))?;
    }
    Ok(actions)
}

/// `speaking_model` and `voice` as the menu's own voice choice, checked against what this Mac has (`state`).
pub fn speech_voice_action(args: &Value, state: &ui::speech_voice::SpeechVoiceState, all_presets: &[String]) -> Result<Option<ui::speech_voice::SpeechVoiceAction>> {
    use ui::speech_voice::{LocalVoice, SpeechVoiceAction as A};
    let Some(model) = args["speaking_model"].as_str() else { return Ok(None) };
    let voice = args["voice"].as_str();
    let find = |voices: &[ui::speech_voice::ApiVoice], what: &str| -> Result<A> {
        let v = voice.with_context(|| format!("NOT_CHANGED: {what} needs a voice: one of {}", names(voices)))?;
        voices
            .iter()
            .find(|x| x.id == v || x.name.eq_ignore_ascii_case(v))
            .map(|x| (x.id.clone(), x.name.clone()))
            .map(|(id, name)| if what == "own_engine" { A::SetOwnVoice { id, name } } else { A::SetElevenLabsVoice { id, name } })
            .with_context(|| format!("NOT_CHANGED: {what} has no voice {v}; its voices: {}", names(voices)))
    };
    Ok(Some(match model {
        "drama3" => {
            ensure!(state.drama3_installed && !crate::edition::PUBLIC, "NOT_CHANGED: Drama 3 is not on this Mac");
            ensure!(voice.is_none(), "NOT_CHANGED: Drama 3 has one voice");
            A::SetLocal(LocalVoice::Drama3)
        }
        "supertonic" => {
            ensure!(!crate::edition::PUBLIC, "NOT_CHANGED: Supertonic is not in this edition");
            A::SetLocal(LocalVoice::Supertonic)
        }
        "phonon" => match voice {
            None => A::SetLocal(LocalVoice::Phonon),
            Some(v) => {
                let name = all_presets.iter().chain(state.presets.iter().map(|p| &p.name)).find(|p| p.eq_ignore_ascii_case(v)).with_context(|| {
                    format!("NOT_CHANGED: Phonon S has no voice {v}; suggested: {}", ui::speech_voice::SUGGESTED_PRESETS.join(", "))
                })?;
                A::SetPreset(name.clone())
            }
        },
        "own_engine" => {
            ensure!(state.own_engine.endpoint.is_some(), "NOT_CHANGED: no own voice engine is connected");
            find(&state.own_engine.voices, "own_engine")?
        }
        "elevenlabs" => {
            ensure!(state.elevenlabs.connected, "NOT_CHANGED: ElevenLabs is not connected");
            find(&state.elevenlabs.voices, "elevenlabs")?
        }
        other => bail!("NOT_CHANGED: unknown speaking model {other}"),
    }))
}

fn names(voices: &[ui::speech_voice::ApiVoice]) -> String {
    voices.iter().map(|v| v.name.as_str()).take(20).collect::<Vec<_>>().join(", ")
}

/// The speaking model and voice as one phrase ("Phonon S · Bryony"), the Speaking caption's words.
pub fn voice_line(state: &ui::speech_voice::SpeechVoiceState) -> String {
    if state.own {
        return format!("own_engine · {}", state.own_engine.selected.as_ref().map_or("no voice", |(_, n)| n.as_str()));
    }
    if state.api {
        return format!("elevenlabs · {}", state.elevenlabs.selected.as_ref().map_or("no voice", |(_, n)| n.as_str()));
    }
    match state.local {
        ui::speech_voice::LocalVoice::Phonon => format!("phonon · {}", state.preset),
        ui::speech_voice::LocalVoice::Drama3 => "drama3".into(),
        ui::speech_voice::LocalVoice::Supertonic => "supertonic".into(),
    }
}

/// The voices and marks on offer: `settings get` for `voice`, `speaking_model` and `icon`, and `personalise` without
/// arguments.
pub fn choices(state: &ui::speech_voice::SpeechVoiceState) -> String {
    let mut models = vec!["phonon (voices: ".to_string() + &ui::speech_voice::SUGGESTED_PRESETS.join(", ") + ", or any Phonon S preset)"];
    if state.drama3_installed && !crate::edition::PUBLIC {
        models.insert(0, "drama3".into());
    }
    if state.own_engine.endpoint.is_some() {
        models.push(format!("own_engine (voices: {})", names(&state.own_engine.voices)));
    }
    if state.elevenlabs.connected {
        models.push(format!("elevenlabs (voices: {})", names(&state.elevenlabs.voices)));
    }
    format!(
        "Icons: helmet, {}, any SF Symbol name, an absolute PNG/PDF/SVG path, or icon_svg. Speaking models: {}.",
        ui::identity::BUNDLED.join(", "),
        models.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_default_identity_publishes_heimdalls_own_names() {
        let d = Identity::default();
        assert!(asr_words(&d).is_empty(), "Heimdall is in the bundled vocabulary already");
        let c = change(&d, &d);
        assert!(!c.names && !c.asr);
    }

    fn apply_args(identity: &mut Identity, args: &Value, data: &Path) -> Result<()> {
        for a in identity_actions(identity, args, data)? {
            identity.apply(&a).unwrap();
        }
        Ok(())
    }

    #[test]
    fn ivy_and_evelyn_through_the_tool_arguments() {
        let t = tempfile::tempdir().unwrap();
        let mut i = Identity::default();
        apply_args(&mut i, &json!({"name":"Ivy","user_name":"Evelyn","icon":"leaf"}), t.path()).unwrap();
        assert_eq!((i.name.as_str(), i.user_name.as_str()), ("Ivy", "Evelyn"));
        assert_eq!(i.icon, Icon::Symbol { name: "leaf".into() });
        assert_eq!(i.wake_name(), ["ivy"]);
        assert_eq!(asr_words(&i), ["Ivy"]);
        let c = change(&Identity::default(), &i);
        assert!(c.names && c.asr);
        // An older caller's wake words change nothing: the wake is the name.
        let before = i.clone();
        apply_args(&mut i, &json!({"wake_words":["Hey Ivy","Ivy","Ivey"]}), t.path()).unwrap();
        assert_eq!((i.clone(), asr_words(&i)), (before, vec!["Ivy".to_string()]));
        // A refused value changes nothing at all.
        let before = i.clone();
        assert!(apply_args(&mut i, &json!({"name":"Iris","user_name":""}), t.path()).is_err());
        assert_eq!(i, before);
        // The agent's own drawing is kept under the data folder, by content.
        let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path d="M8 1 L15 15 L1 15 Z"/></svg>"#;
        apply_args(&mut i, &json!({"icon_svg":svg}), t.path()).unwrap();
        let Icon::Image { path } = &i.icon else { panic!("{:?}", i.icon) };
        assert!(path.starts_with(t.path().join("identity")) && std::fs::read_to_string(path).unwrap() == svg);
        assert!(apply_args(&mut i, &json!({"icon_svg":"<svg><script>x</script></svg>"}), t.path()).is_err());
        for bad in [r#"<?xml version="1.0"?><!DOCTYPE svg [<!ENTITY a "aaaa">]><svg>&a;</svg>"#, r#"<svg><image href="file:///etc/x.png"/></svg>"#,
            r#"<svg><a href = "https://x"><path d="M0 0"/></a></svg>"#, r#"<svg onload="x()"><path d="M0 0"/></svg>"#] {
            assert!(apply_args(&mut i, &json!({"icon_svg":bad}), t.path()).is_err(), "{bad}");
        }
        assert!(plain_shapes(r##"<svg><use href="#leaf"/></svg>"##));
        // A second drawing replaces the first on disk.
        let other = svg.replace("M8 1", "M8 2");
        apply_args(&mut i, &json!({"icon_svg":other}), t.path()).unwrap();
        assert_eq!(std::fs::read_dir(t.path().join("identity")).unwrap().count(), 1);
        assert!(apply_args(&mut i, &json!({"icon":"/nonexistent/mark.svg"}), t.path()).is_err());
        assert!(describe(&i, "phonon · Bryony").starts_with("Name Ivy · user Evelyn · icon /"));
    }

    #[test]
    fn speaking_models_map_to_the_menus_choices_and_refuse_what_is_missing() {
        use ui::speech_voice::{demo_speech_voice, LocalVoice, SpeechVoiceAction as A};
        let s = demo_speech_voice(false, LocalVoice::Drama3, true);
        let pick = |a: Value| speech_voice_action(&a, &s, &[]);
        assert_eq!(pick(json!({})).unwrap(), None);
        assert_eq!(pick(json!({"speaking_model":"phonon","voice":"bryony"})).unwrap(), Some(A::SetPreset("Bryony".into())));
        assert!(pick(json!({"speaking_model":"phonon","voice":"Nobody"})).is_err());
        assert_eq!(pick(json!({"speaking_model":"elevenlabs","voice":"Rachel"})).unwrap(), Some(A::SetElevenLabsVoice { id: "21m00Tcm4TlvDq8ikWAM".into(), name: "Rachel".into() }));
        assert_eq!(pick(json!({"speaking_model":"own_engine","voice":"af_bella"})).unwrap(), Some(A::SetOwnVoice { id: "af_bella".into(), name: "Bella".into() }));
        let public = demo_speech_voice(false, LocalVoice::Phonon, false);
        assert!(speech_voice_action(&json!({"speaking_model":"drama3"}), &public, &[]).is_err(), "no Drama 3 where it is not installed");
        assert!(speech_voice_action(&json!({"speaking_model":"elevenlabs","voice":"George"}), &public, &[]).is_err());
    }
}
