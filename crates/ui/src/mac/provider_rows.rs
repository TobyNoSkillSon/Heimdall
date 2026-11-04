//! Agent ▸ Model ▸ Add Provider's rows, the same in both editions, in this order: Local Splash Engine, Custom API (a
//! submenu: OpenAI-Compatible API…, Anthropic-Compatible API…), OpenCode Go subscription…, Claude Code subscription,
//! Anthropic API…, Codex subscription…. Each row is the provider's title as Providers lists it, how it is reached
//! included. Nothing added is offered again: a connected provider leaves the list (it is under
//! Providers), the built-in engine leaves it while it is added, the agent's or running, and the Claude relay once it is
//! added (`Settings::relay_added`; it was in Cloud and in Add Provider at once). Adding either never
//! makes it the agent. Also the licence lines of the built-in engine's models (`add_licence_rows`).

use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSMenu, NSMenuItem};
use objc2_foundation::MainThreadMarker;

use super::menu::AddEntry;
use super::util::*;
use crate::{ApiProviders, EngineState, EngineStatus, ProviderAuth};

pub(crate) const CUSTOM_PROVIDER: &str = "Custom API";
pub(crate) const CLAUDE_VIA_T3: &str = crate::model::RELAY_TITLE;
pub(crate) const OPENCODE_GO: &str = "OpenCode Go subscription";
pub(crate) const ANTHROPIC_KEY: &str = "Anthropic API";
pub(crate) const CHATGPT_SUBSCRIPTION: &str = "Codex subscription";
/// The custom provider's two kinds, in the choice's order: (provider id, row title).
pub(crate) const CUSTOM_KINDS: [(&str, &str); 2] = [("openai-compatible", "OpenAI-Compatible API"), ("anthropic-compatible", "Anthropic-Compatible API")];

/// Add Provider's rows in their order, as titles without the trailing ellipsis (the order the user set for both editions).
pub(crate) fn add_order() -> [&'static str; 6] {
    [super::menu::BUILT_IN_ENGINE, CUSTOM_PROVIDER, OPENCODE_GO, CLAUDE_VIA_T3, ANTHROPIC_KEY, CHATGPT_SUBSCRIPTION]
}

/// Whether the built-in engine is in use, so it belongs under Providers rather than Add Provider.
pub(crate) fn engine_in_use(api: &ApiProviders, engine: &EngineStatus) -> bool {
    api.engine_added || api.local_chosen() || engine.state != EngineState::Off
}

/// Add Provider's one untitled group (`menu::add_provider_section`): the rows still to add, in `add_order`.
pub(crate) fn add_groups<'a>(api: &ApiProviders, engine: Option<&EngineStatus>, target: Option<&'a AnyObject>, mtm: MainThreadMarker)
    -> Vec<(&'static str, Vec<AddEntry<'a>>)> {
    let missing = |id: &str| api.providers.iter().position(|p| p.id == id && p.auth == ProviderAuth::Missing);
    let entry = |title: String, icon: &str, action: objc2::runtime::Sel, tag: isize, notice: Option<String>| AddEntry {
        title, icon: icon.into(), action, tag, target, notice, submenu: None, enabled: true,
    };
    let provider = |id: &str, title: &str| missing(id).map(|i| {
        let p = &api.providers[i];
        entry(format!("{title}\u{2026}"), &p.icon, sel!(connectProvider:), i as isize, p.notice.clone())
    });
    let mut rows = vec![];
    // The built-in engine: added as a provider; the agent's model stays (`UiAction::AddBuiltInEngine`).
    if let Some(engine) = engine.filter(|e| !engine_in_use(api, e)) {
        rows.push(AddEntry { enabled: !engine.models.is_empty(), ..entry(super::menu::BUILT_IN_ENGINE.into(), "local", sel!(addBuiltInEngine:), 0, None) });
    }
    // Custom provider: first the kind, then its URL and key in the connect panel.
    if let Some(choice) = custom_kinds_menu(api, true, target, mtm) {
        rows.push(AddEntry { submenu: Some(choice), ..entry(CUSTOM_PROVIDER.into(), "custom", sel!(connectProvider:), -1, None) });
    }
    rows.extend(provider("opencode-go", OPENCODE_GO));
    // The Claude relay: added, its row joins Cloud and it leaves this list (docs/claude-relay.md).
    if !api.relay_added && !api.relay_on { rows.push(entry(CLAUDE_VIA_T3.into(), "anthropic", sel!(claudeViaT3:), 0, None)); }
    rows.extend(provider("anthropic", ANTHROPIC_KEY));
    rows.extend(provider("openai", CHATGPT_SUBSCRIPTION));
    vec![("", rows)]
}

/// The custom provider's kinds (OpenAI-Compatible…, Anthropic-Compatible…), each opening its connect panel
/// (`connectProvider:`, tagged with the provider's index). `only_missing`: the kinds not connected yet, as Add Provider
/// lists them; Setup offers both. None when there is nothing to offer.
pub(crate) fn custom_kinds_menu(api: &ApiProviders, only_missing: bool, target: Option<&AnyObject>, mtm: MainThreadMarker) -> Option<objc2::rc::Retained<NSMenu>> {
    let kinds: Vec<(usize, &str)> = custom_kinds(api, only_missing);
    if kinds.is_empty() { return None; }
    let choice = NSMenu::new(mtm);
    choice.setAutoenablesItems(false);
    for (i, title) in kinds {
        let item = menu_item(&format!("{title}\u{2026}"), None, Some(sel!(connectProvider:)), "", target, mtm);
        item.setTag(i as isize);
        choice.addItem(&item);
    }
    Some(choice)
}

/// The custom kinds as (index in `api.providers`, title), in `CUSTOM_KINDS` order.
pub(crate) fn custom_kinds(api: &ApiProviders, only_missing: bool) -> Vec<(usize, &'static str)> {
    CUSTOM_KINDS.iter().filter_map(|(id, title)| {
        api.providers.iter().position(|p| p.id == *id && (!only_missing || p.auth == ProviderAuth::Missing)).map(|i| (i, *title))
    }).collect()
}

/// A model's licence, by the built-in engine's model id: (the line the menu shows, the file under the app's
/// `Contents/Resources/licenses`). A model adds its own here.
pub(crate) fn model_licence(id: &str) -> Option<(&'static str, &'static str)> {
    match id {
        "qwen3.8-27b" => Some(("Apache-2.0 \u{b7} Copyright 2026 Alibaba Cloud", "Qwen3.8-Apache-2.0.txt")),
        _ => None,
    }
}

/// Under the built-in engine's models: each licensed model's licence line and Licence…, which opens the bundled text
/// (`openModelLicence:`, tagged with the model's index in `engine.models`).
pub(crate) fn add_licence_rows(menu: &NSMenu, engine: &EngineStatus, target: Option<&AnyObject>, mtm: MainThreadMarker) {
    let licensed: Vec<(usize, &str)> = engine.models.iter().enumerate().filter_map(|(i, m)| model_licence(&m.id).map(|(line, _)| (i, line))).collect();
    if licensed.is_empty() { return; }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let many = licensed.len() > 1;
    for (i, line) in licensed {
        let text = if many { format!("{} \u{b7} {line}", engine.models[i].label) } else { line.to_owned() };
        menu.addItem(&caption_item(&text, mtm));
        let item = menu_item("Licence\u{2026}", None, Some(sel!(openModelLicence:)), "", target, mtm);
        item.setTag(i as isize);
        menu.addItem(&item);
    }
}

/// The bundled licence file of a model, when the app runs from its bundle (`bundle` ends in `.app`) and the file is
/// there; nothing otherwise (a build or test run opens nothing).
pub(crate) fn licence_path(bundle: &std::path::Path, resources: &std::path::Path, file: &str) -> Option<std::path::PathBuf> {
    if bundle.extension().is_none_or(|e| e != "app") { return None; }
    let path = resources.join("licenses").join(file);
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwen_has_its_licence_and_unknown_models_none() {
        assert_eq!(model_licence("qwen3.8-27b"), Some(("Apache-2.0 \u{b7} Copyright 2026 Alibaba Cloud", "Qwen3.8-Apache-2.0.txt")));
        assert_eq!(model_licence("other"), None);
    }

    #[test]
    fn a_licence_opens_only_from_the_app_bundle() {
        let d = std::env::temp_dir().join(format!("heimdall-licence-{}", std::process::id()));
        let app = d.join("Heimdall Alpha.app");
        let resources = app.join("Contents/Resources");
        std::fs::create_dir_all(resources.join("licenses")).unwrap();
        assert_eq!(licence_path(&app, &resources, "Qwen3.8-Apache-2.0.txt"), None, "no file, nothing to open");
        std::fs::write(resources.join("licenses/Qwen3.8-Apache-2.0.txt"), "Apache License").unwrap();
        assert_eq!(licence_path(&app, &resources, "Qwen3.8-Apache-2.0.txt"), Some(resources.join("licenses/Qwen3.8-Apache-2.0.txt")));
        assert_eq!(licence_path(&d.join("target/debug"), &resources, "Qwen3.8-Apache-2.0.txt"), None, "not a bundle");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn wizard_provider_rows_do_what_the_add_provider_rows_do() {
        use crate::{ProviderAction, ProviderChoice};
        // Setup's four rows are Add Provider's rows without the API-key ones, by title and in order.
        let titles: Vec<_> = ProviderChoice::ALL.iter().map(|c| c.title()).collect();
        let keyless: Vec<_> = add_order().into_iter().filter(|t| *t != CUSTOM_PROVIDER && *t != ANTHROPIC_KEY).collect();
        assert_eq!(titles, keyless);
        let actions: Vec<_> = ProviderChoice::ALL.iter().map(|c| c.menu_action()).collect();
        assert_eq!(actions, [
            ProviderAction::Engine,
            ProviderAction::Connect("opencode-go"),
            ProviderAction::ClaudeViaT3,
            ProviderAction::Connect("openai"),
        ]);
        // The custom kinds: the menu lists those still missing, Setup both, each by its provider's index.
        let provider = |id: &str, auth| crate::ApiProviderRow {
            id: id.into(), name: id.into(), icon: String::new(), console_url: String::new(), auth, notice: None, sign_in: false,
            credit: None, spend: None, custom_url: true, key_required: false, endpoint: None, group: Default::default(), key_fallback: false,
        };
        let api = ApiProviders {
            providers: vec![provider("openai", ProviderAuth::Missing), provider("anthropic-compatible", ProviderAuth::Missing), provider("openai-compatible", ProviderAuth::Connected)],
            ..Default::default()
        };
        assert_eq!(custom_kinds(&api, false), [(2, "OpenAI-Compatible API"), (1, "Anthropic-Compatible API")]);
        assert_eq!(custom_kinds(&api, true), [(1, "Anthropic-Compatible API")]);
    }

    #[test]
    fn add_provider_order_is_the_six_rows() {
        assert_eq!(add_order(), ["Local Splash Engine", "Custom API", "OpenCode Go subscription", "Claude Code subscription", "Anthropic API", "Codex subscription"]);
    }

    /// a provider is named the same in Add Provider and under Providers, and says how it is
    /// reached (API or subscription).
    #[test]
    fn add_provider_rows_carry_the_providers_own_titles() {
        let api = crate::demo_all_providers(ProviderAuth::Connected);
        let title = |id: &str| api.providers.iter().find(|p| p.id == id).map(crate::ApiProviderRow::title).unwrap();
        assert_eq!(title("openai"), CHATGPT_SUBSCRIPTION);
        assert_eq!(title("opencode-go"), OPENCODE_GO);
        assert_eq!(title("anthropic"), ANTHROPIC_KEY);
        for (id, kind) in CUSTOM_KINDS { assert_eq!(title(id), kind); }
        for t in add_order() {
            assert!(t.ends_with(" API") || t.ends_with(" subscription") || t.starts_with("Local "), "{t} says how it is reached");
        }
    }
}
