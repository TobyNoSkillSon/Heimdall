//! The app's side of `crates/keychain` (cleanup night 14 Nov): which store a run gets, and what the user sees when the
//! Keychain stops answering.
//!
//! Every offline mode (replay, night diagnostics, lab scenario, remote lab, idle, settings) and any run with an explicit
//! `--data-dir` keeps its secrets in `<data>/lab-secrets/` (memory without a data folder) and never reads the login
//! Keychain, even from the installed bundle. A blocked or refused Keychain item turns into one red island pill per
//! feature ("Keychain not answering · T3"), recorded as a health diagnostic, until the item answers again, which is
//! shown green; the live loop never waits on it (`keychain::READ_WAIT`).
use std::path::Path;

/// Chooses the isolated store before anything reads a secret. The live app on its own profile keeps the automatic
/// rule (the installed bundle: the Keychain; anything else: off).
pub fn isolate(live_app: bool, data: Option<&Path>) {
    if let Some(isolation) = isolation(live_app, data) {
        keychain::isolate(isolation);
    }
}
fn isolation(live_app: bool, data: Option<&Path>) -> Option<keychain::Isolation> {
    match data {
        Some(d) => Some(keychain::Isolation::Dir(d.join("lab-secrets"))),
        None if !live_app => Some(keychain::Isolation::Memory),
        None => None,
    }
}

/// What a Keychain service is for, in the user's words.
pub fn feature(service: &str) -> String {
    let named = |prefix: &str| service.strip_prefix(prefix).map(str::to_owned);
    if service == t3::client::KEYCHAIN_SERVICE {
        return "T3".into();
    }
    named("Heimdall API provider: ")
        .or_else(|| named("Heimdall Alpha API provider: "))
        .or_else(|| named("Heimdall voice provider: ").map(|v| format!("{v} voice")))
        .or_else(|| named("Heimdall Alpha voice provider: ").map(|v| format!("{v} voice")))
        .unwrap_or_else(|| if service.contains("friends") { "Friends".into() } else { service.into() })
}

/// The pill's words for why an item does not answer: (label, what to do). A locked Keychain is fixed by unlocking it; an
/// item this app may not read (an earlier build saved it, and macOS no longer trusts the new signature) is not, and is
/// fixed by saving the key again (VM run 14 Nov, upgrade from alpha.1: errSecAuthFailed).
pub fn words(service: &str, cause: keychain::Cause) -> (String, String) {
    let what = feature(service);
    let reenter = if service.contains("provider: ") { "re-enter the key in Settings \u{25b8} Providers" } else { "sign in to it again" };
    match cause {
        keychain::Cause::Blocked => (format!("Keychain not answering \u{b7} {what}"), "Unlock the login Keychain or close its password prompt".into()),
        keychain::Cause::Locked => (format!("Keychain locked \u{b7} {what}"), format!("Unlock the login Keychain; if it is unlocked, {reenter}")),
        keychain::Cause::Unreadable => (format!("Saved key unreadable \u{b7} {what}"), format!("This build can\u{2019}t read the key an earlier build saved: {reenter}")),
        keychain::Cause::Failed => (format!("Secret store failed \u{b7} {what}"), format!("Check the lab secrets folder, or {reenter}")),
    }
}

pub fn cue(event: &keychain::Event) -> ui::HealthCue {
    match event {
        keychain::Event::Degraded { service, cause, .. } => {
            let (label, action) = words(service, *cause);
            ui::HealthCue::Problem { key: format!("keychain:{service}"), part: ui::Part::Settings, label, action, control: ui::HealthControl::None }
        }
        keychain::Event::Recovered { service } => ui::HealthCue::Recovered {
            key: format!("keychain:{service}"),
            part: ui::Part::Settings,
            label: format!("{} back", feature(service)),
        },
    }
}

/// The live app: every Keychain event to the island and the health diagnostics.
pub fn report(u: ui::Ui, tools: std::sync::Arc<crate::bridge::Tools>) {
    eprintln!("secrets: {:?} store", keychain::kind());
    // Reported off the caller's thread (a timeout is reported from the thread that asked, which can be the live
    // loop), by one thread so a pill and its recovery keep their order.
    let (tx, rx) = std::sync::mpsc::channel::<keychain::Event>();
    std::thread::Builder::new()
        .name("keychain-notice".into())
        .spawn(move || {
            for event in rx {
                let (service, state, reason) = match &event {
                    keychain::Event::Degraded { service, reason, .. } => (service, "degraded", reason.as_str()),
                    keychain::Event::Recovered { service } => (service, "recovered", ""),
                };
                tools.record_diagnostic("health", serde_json::json!({"component":"keychain", "service":service, "state":state, "reason":reason}));
                u.send(ui::UiUpdate::Health(cue(&event)));
            }
        })
        .expect("keychain notice thread");
    let tx = std::sync::Mutex::new(tx);
    keychain::on_event(move |event| {
        // A Keychain that answers again ends a stall the agent's state may not show (the live loop froze while
        // Ready): wakes and notices that waited go now (review 14 Nov).
        if matches!(event, keychain::Event::Recovered { .. }) {
            agent::wakes::agent_ready();
        }
        let _ = tx.lock().unwrap_or_else(|p| p.into_inner()).send(event.clone());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_modes_and_any_data_dir_never_get_the_keychain() {
        assert!(matches!(isolation(false, None), Some(keychain::Isolation::Memory)));
        let lab = Path::new("/tmp/Heimdall-lab/night-1");
        assert!(matches!(isolation(false, Some(lab)), Some(keychain::Isolation::Dir(d)) if d == lab.join("lab-secrets")));
        // The release candidate's verify runs the bundle with --data-dir: isolated too.
        assert!(matches!(isolation(true, Some(lab)), Some(keychain::Isolation::Dir(_))));
        assert!(isolation(true, None).is_none(), "only the live app on its own profile keeps the automatic rule");
    }

    #[test]
    fn a_blocked_item_is_one_named_pill_until_it_answers() {
        let service = t3::client::KEYCHAIN_SERVICE.to_owned();
        match cue(&keychain::Event::Degraded { service: service.clone(), reason: "did not answer".into(), cause: keychain::Cause::Blocked }) {
            ui::HealthCue::Problem { key, label, .. } => {
                assert_eq!(key, format!("keychain:{service}"));
                assert_eq!(label, "Keychain not answering · T3");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(cue(&keychain::Event::Recovered { service: service.clone() }), ui::HealthCue::Recovered { key, .. } if key == format!("keychain:{service}")));
        assert_eq!(feature(crate::speech_voice::KEYCHAIN_SERVICE), "ElevenLabs");
        assert_eq!(feature(crate::own_voice::KEYCHAIN_SERVICE), "Own Engine voice");
        assert_eq!(feature(agent::provider::PROVIDERS[0].keychain_service), agent::provider::PROVIDERS[0].keychain_service.rsplit(": ").next().unwrap());
        assert_eq!(feature("Heimdall friends identity"), "Friends");
    }

    /// VM run 14 Nov: after the upgrade the pill said "Keychain not answering" and told the user to unlock the Keychain,
    /// which cannot help an item the old build saved. Each cause says its own fix.
    #[test]
    fn a_locked_keychain_and_an_unreadable_item_say_different_fixes() {
        let service = "Heimdall Alpha API provider: OpenAI-Compatible";
        let (label, action) = words(service, keychain::Cause::Unreadable);
        assert_eq!(label, "Saved key unreadable \u{b7} OpenAI-Compatible");
        assert!(action.contains("re-enter the key in Settings \u{25b8} Providers") && !action.contains("Unlock"), "{action}");
        let (label, action) = words(service, keychain::Cause::Locked);
        assert_eq!(label, "Keychain locked \u{b7} OpenAI-Compatible");
        assert!(action.starts_with("Unlock the login Keychain") && action.contains("re-enter the key"), "{action}");
        let (label, action) = words(service, keychain::Cause::Blocked);
        assert_eq!(label, "Keychain not answering \u{b7} OpenAI-Compatible");
        assert!(action.contains("password prompt"), "{action}");
        assert!(words(t3::client::KEYCHAIN_SERVICE, keychain::Cause::Unreadable).1.contains("sign in to it again"));
    }
}
