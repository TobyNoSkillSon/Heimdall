//! Offscreen renders that need no menu, window or status item on screen: the menu-bar glyph in every state on
//! light and dark bars at 1× and 2×, the app icon at every size macOS reads (and its `.icns`), the real tile
//! block drawn with Listening on and off, the menu's first frame (tiles and the line under them) in every engine
//! state, and the Seeing ▸ Recording and Inference Engine submenus' real rows.

use std::path::Path;

use objc2::rc::Retained;
use objc2::AllocAnyThread as _;
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization as _, NSAppearanceNameAqua, NSAppearanceNameDarkAqua, NSApplication,
    NSApplicationActivationPolicy, NSBitmapImageRep, NSColor, NSView,
};
use objc2_foundation::{MainThreadMarker, NSSize};

use super::controls::{LabelRow, TileRow, TileView};
use crate::model::{EngineModel, EngineState, EngineStatus, TileLook};
use super::helmet;
use super::util::rect;

/// The menu's real width (lab/ui-renders/menu.png is 624 px at 2×).
pub(super) const MENU_WIDTH: f64 = 312.0;

/// Writes every render into `dir`; with `icns`, also builds the app icon there from the rendered `.iconset`.
/// Returns what failed.
pub(crate) fn render(dir: &Path, icns: Option<&Path>) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("render_offscreen runs on the main thread");
    // No Dock icon, no activation: this process only draws.
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let wrote = |path: &Path, ok: bool, failures: &mut Vec<String>| {
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
        if !ok {
            failures.push(path.display().to_string());
        }
    };

    for (state, look) in [("on", helmet::BarState::On), ("off", helmet::BarState::Off), ("attention", helmet::BarState::Attention)] {
        for (appearance, dark) in [("light", false), ("dark", true)] {
            for scale in [1.0, 2.0] {
                let path = dir.join(format!("menubar-{state}-{appearance}@{scale}x.png"));
                let ok = helmet::write_menu_bar(&path, look, dark, scale);
                wrote(&path, ok, &mut failures);
            }
        }
    }

    let svg = dir.join("Heimdall.svg");
    wrote(&svg, std::fs::write(&svg, helmet::app_icon_svg()).is_ok(), &mut failures);
    let iconset = dir.join("Heimdall.iconset");
    let missing = helmet::write_iconset(&iconset);
    wrote(&iconset, missing.is_empty(), &mut failures);
    if let (Some(icns), true) = (icns, missing.is_empty()) {
        let ok = std::process::Command::new("/usr/bin/iconutil")
            .args(["-c", "icns", "-o"])
            .arg(icns)
            .arg(&iconset)
            .status()
            .is_ok_and(|s| s.success());
        wrote(icns, ok, &mut failures);
    }

    // Show Islands on Streams off (the default) and on.
    for (name, on) in [("menu-islands-streams-off", false), ("menu-islands-streams-on", true)] {
        for (appearance, dark) in [("dark", true), ("light", false)] {
            let path = dir.join(format!("{name}-{appearance}.png"));
            let ok = stack(&path, &super::menu::islands_streams_preview(on, mtm), dark, mtm);
            wrote(&path, ok, &mut failures);
        }
    }

    // Saved states: everything on; and a mix, so a faded tile still shows which way it was left. The engine
    // runs in the first two and is off in the mixes.
    // Show Islands on and off (⌃⌘,).
    for (name, shown) in [("menu-islands-shown", true), ("menu-islands-hidden", false)] {
        for (appearance, dark) in [("dark", true), ("light", false)] {
            let path = dir.join(format!("{name}-{appearance}.png"));
            let ok = stack(&path, &super::menu::islands_section_preview(shown, mtm), dark, mtm);
            wrote(&path, ok, &mut failures);
        }
    }

    // Memory ▸ (26 Oct): Heimdall's total with Drama 3 loaded, and its parts.
    failures.extend(memory(dir, mtm));

    // Shortcuts ▸ (24 Oct): each global shortcut with its keys; Listening on, Seeing off, the islands shown.
    for (appearance, dark) in [("dark", true), ("light", false)] {
        let path = dir.join(format!("menu-shortcuts-{appearance}.png"));
        let ok = shortcuts_submenu(&path, dark, mtm);
        wrote(&path, ok, &mut failures);
    }

    let all_on = [true, true, true];
    let mixed = [true, false, true];
    for (name, listening, saved, engine) in [
        ("listening-on", true, all_on, TileLook::On),
        ("listening-off", false, all_on, TileLook::On),
        ("listening-off-mixed", false, mixed, TileLook::Off),
        ("listening-on-mixed", true, mixed, TileLook::Off),
    ] {
        for (appearance, dark) in [("dark", true), ("light", false)] {
            let path = dir.join(format!("tiles-{name}-{appearance}.png"));
            let ok = tiles(&path, listening, saved, engine, false, dark, mtm);
            wrote(&path, ok, &mut failures);
        }
    }
    // The menu's first frame, before any interaction, in every engine state with Listening on and off: the tiles
    // and, with Listening off, the grey line under them. Then the engine's submenu in each state.
    // Inference Engine ▸ API Providers in each connection state, its Add Model picker (loaded, loading, failed),
    // the connected provider's own submenu, and the key panel as Connect opens it.
    use crate::{demo_api_providers, ProviderAuth};
    let rejected = ProviderAuth::Problem("OpenCode Go: access rejected / no subscription".into());
    for (name, api) in [
        ("not-connected", demo_api_providers(ProviderAuth::Missing, false)),
        ("connected", demo_api_providers(ProviderAuth::Connected, false)),
        ("rejected", demo_api_providers(rejected.clone(), false)),
        ("local-selected", demo_api_providers(ProviderAuth::Connected, true)),
    ] {
        for (appearance, dark) in [("dark", true), ("light", false)] {
            let path = dir.join(format!("menu-api-{name}-{appearance}.png"));
            let ok = stack(&path, &super::menu::api_providers_menu(&api, &Default::default(), None, None, None, None, mtm).0, dark, mtm);
            wrote(&path, ok, &mut failures);
        }
    }
    let loading = crate::ApiProviders { catalog: vec![], ..demo_api_providers(ProviderAuth::Connected, false) };
    let failed = crate::ApiProviders { catalog_problem: Some("OpenCode Go unreachable".into()), ..loading.clone() };
    for (name, api, dark) in [
        ("menu-api-add-model", demo_api_providers(ProviderAuth::Connected, false), true),
        ("menu-api-add-model-light", demo_api_providers(ProviderAuth::Connected, false), false),
        ("menu-api-add-model-loading", loading, true),
        ("menu-api-add-model-failed", failed, true),
    ] {
        let path = dir.join(format!("{name}.png"));
        let (menu, _) = super::menu::api_providers_menu(&api, &Default::default(), None, None, None, None, mtm);
        // [providers] The picker lives in its provider's submenu now.
        let picker = menu.itemArray().iter().find(|i| i.title().to_string() == "OpenCode Go subscription").and_then(|i| i.submenu())
            .and_then(|own| own.itemArray().iter().find(|i| i.title().to_string() == "Add Model").and_then(|i| i.submenu()));
        let ok = picker.is_some_and(|p| stack(&path, &p, dark, mtm));
        wrote(&path, ok, &mut failures);
    }
    for (name, auth) in [("connected", ProviderAuth::Connected), ("rejected", rejected)] {
        let path = dir.join(format!("menu-api-provider-{name}.png"));
        let (menu, _) = super::menu::api_providers_menu(&demo_api_providers(auth, false), &Default::default(), None, None, None, None, mtm);
        let own = menu.itemArray().iter().find(|i| i.title().to_string() == "OpenCode Go subscription").and_then(|i| i.submenu());
        let ok = own.is_some_and(|p| stack(&path, &p, true, mtm));
        wrote(&path, ok, &mut failures);
    }
    // Anthropic beside OpenCode Go: not connected, connected with Opus selected and its details open (slider and
    // price), its own submenu (credits, Replace Key…), its live list in the picker.
    let opus: std::collections::BTreeSet<(String, String)> = [("anthropic".to_string(), "claude-opus-5-5".to_string())].into();
    for (name, auth, expanded, dark) in [
        ("menu-api-anthropic-not-connected", ProviderAuth::Missing, Default::default(), true),
        ("menu-api-anthropic-connected", ProviderAuth::Connected, Default::default(), true),
        ("menu-api-anthropic-expanded", ProviderAuth::Connected, opus.clone(), true),
        ("menu-api-anthropic-expanded-light", ProviderAuth::Connected, opus.clone(), false),
        ("menu-api-anthropic-expanded-all", ProviderAuth::Connected, [("anthropic", "claude-opus-5-5"), ("anthropic", "claude-sonnet-5-5"), ("anthropic", "claude-haiku-5-5"), ("opencode-go", "claude-haiku-5-5")].iter().map(|(p, i)| (p.to_string(), i.to_string())).collect(), true),
        ("menu-api-anthropic-expanded-all-light", ProviderAuth::Connected, [("anthropic", "claude-opus-5-5"), ("anthropic", "claude-sonnet-5-5"), ("anthropic", "claude-haiku-5-5"), ("opencode-go", "claude-haiku-5-5")].iter().map(|(p, i)| (p.to_string(), i.to_string())).collect(), false),
        ("menu-api-anthropic-credits-used", ProviderAuth::Problem("Anthropic credits used up".into()), Default::default(), true),
    ] {
        let path = dir.join(format!("{name}.png"));
        let ok = stack(&path, &super::menu::api_providers_menu(&crate::demo_anthropic_providers(auth), &expanded, None, None, None, None, mtm).0, dark, mtm);
        wrote(&path, ok, &mut failures);
    }
    let (menu, _) = super::menu::api_providers_menu(&crate::demo_anthropic_providers(ProviderAuth::Connected), &Default::default(), None, None, None, None, mtm);
    for (name, title) in [("menu-api-anthropic-provider", "Anthropic API"), ("menu-api-anthropic-add-model", "Add Model")] {
        let path = dir.join(format!("{name}.png"));
        let anthropic = menu.itemArray().iter().find(|i| i.title().to_string() == "Anthropic API").and_then(|i| i.submenu());
        let sub = if title == "Anthropic API" { anthropic } else { anthropic.and_then(|a| a.itemArray().iter().find(|i| i.title().to_string() == title).and_then(|i| i.submenu())) };
        let ok = sub.is_some_and(|p| stack(&path, &p, true, mtm));
        wrote(&path, ok, &mut failures);
    }
    // [providers] night 1022: Agent ▸ Model with every provider, into `providers/`: the sub-table (connected and not,
    // Opus open, dark and light), each provider's own submenu, the Add Model pickers (OpenRouter whole and searched),
    // Local (built-in engine and own engine), the URL panels, and the whole menu's Agent section.
    {
        let out = dir.join("providers");
        std::fs::create_dir_all(&out).ok();
        let engine = engine_states().into_iter().find(|(n, _)| *n == "ready").map(|(_, e)| e).unwrap_or_default();
        let opus: std::collections::BTreeSet<(String, String)> = [("anthropic".to_string(), "claude-opus-5-5".to_string())].into();
        for (name, others, expanded, dark) in [
            ("model-connected", ProviderAuth::Connected, Default::default(), true),
            ("model-connected-light", ProviderAuth::Connected, Default::default(), false),
            ("model-opus-open", ProviderAuth::Connected, opus.clone(), true),
            ("model-others-not-connected", ProviderAuth::Missing, Default::default(), true),
        ] {
            let path = out.join(format!("{name}.png"));
            let ok = stack(&path, &super::menu::api_providers_menu(&crate::demo_all_providers(others), &expanded, Some((&engine, "low")), None, None, None, mtm).0, dark, mtm);
            wrote(&path, ok, &mut failures);
        }
        // The built-in engine selected (no API model).
        let mut local = crate::demo_all_providers(ProviderAuth::Connected);
        local.selected = None;
        let path = out.join("model-built-in-selected.png");
        let ok = stack(&path, &super::menu::api_providers_menu(&local, &Default::default(), Some((&engine, "low")), None, None, None, mtm).0, true, mtm);
        wrote(&path, ok, &mut failures);
        // [menu lane] The local model opened (context up to its window, thinking), Add Provider open (private apart from
        // online, ChatGPT Codex coming soon) and closed, and Background Tasks ▸ Model in its states.
        let mut wide = engine.clone();
        wide.context_limit = Some(262_144);
        let local_open: std::collections::BTreeSet<(String, String)> = [(crate::model::LOCAL_ENGINE.to_string(), wide.model_id.clone())].into();
        for (name, others, expanded, add, dark) in [
            ("model-local-open", ProviderAuth::Connected, local_open.clone(), Some(false), true),
            ("model-local-open-light", ProviderAuth::Connected, local_open.clone(), Some(false), false),
            ("model-add-provider-open", ProviderAuth::Missing, Default::default(), Some(true), true),
            ("model-add-provider-open-light", ProviderAuth::Missing, Default::default(), Some(true), false),
            ("model-add-provider-closed", ProviderAuth::Missing, Default::default(), Some(false), true),
        ] {
            let path = out.join(format!("{name}.png"));
            let ok = stack(&path, &super::menu::api_providers_menu(&crate::demo_all_providers(others), &expanded, Some((&wide, "low")), None, add, None, mtm).0, dark, mtm);
            wrote(&path, ok, &mut failures);
        }
        let haiku = (crate::model::BACKGROUND_DEFAULT.0.to_string(), crate::model::BACKGROUND_DEFAULT.1.to_string());
        for (name, api, selected, dark) in [
            ("background-model", crate::demo_all_providers(ProviderAuth::Connected), haiku.clone(), true),
            ("background-model-light", crate::demo_all_providers(ProviderAuth::Connected), haiku.clone(), false),
            ("background-model-local", crate::demo_all_providers(ProviderAuth::Connected), (crate::model::LOCAL_ENGINE.to_string(), wide.model_id.clone()), true),
            ("background-model-anthropic-missing", crate::demo_anthropic_providers(ProviderAuth::Missing), haiku.clone(), true),
        ] {
            let path = out.join(format!("{name}.png"));
            let settings = crate::Settings { background_model: selected, ..Default::default() };
            let ok = stack(&path, &super::background_picker::build(&api, &wide, &[], &settings, &Default::default(), super::background_picker::Scope::Shared, None, mtm).0, dark, mtm);
            wrote(&path, ok, &mut failures);
        }
        let sub = |menu: &objc2_app_kit::NSMenu, title: &str| menu.itemArray().iter().find(|i| i.title().to_string() == title).and_then(|i| i.submenu());
        for (others, suffix) in [(ProviderAuth::Connected, ""), (ProviderAuth::Missing, "-others-not-connected")] {
            let (menu, _) = super::menu::api_providers_menu(&crate::demo_all_providers(others), &Default::default(), Some((&engine, "low")), None, None, None, mtm);
            for (name, title) in [("provider-anthropic", "Anthropic API"), ("provider-opencode", "OpenCode Go subscription"), ("provider-openai-compatible", "OpenAI-Compatible API"), ("provider-anthropic-compatible", "Anthropic-Compatible API"), ("provider-built-in-engine", super::menu::BUILT_IN_ENGINE)] {
                let Some(own) = sub(&menu, title) else { continue };
                let path = out.join(format!("{name}{suffix}.png"));
                let ok = stack(&path, &own, true, mtm);
                wrote(&path, ok, &mut failures);
                if let Some(picker) = sub(&own, "Add Model") {
                    let path = out.join(format!("{name}-add-model{suffix}.png"));
                    let ok = stack(&path, &picker, true, mtm);
                    wrote(&path, ok, &mut failures);
                }
            }
            if suffix.is_empty() {
                // OpenRouter (added as an OpenAI-compatible provider) searched for "kimi": one row stays; the search field
                // shows the words. A fresh menu: the render above took the first one's views.
                let (menu, rows) = super::menu::api_providers_menu(&crate::demo_all_providers(ProviderAuth::Connected), &Default::default(), Some((&engine, "low")), None, None, None, mtm);
                let picker = sub(&menu, "OpenAI-Compatible API").and_then(|o| sub(&o, "Add Model")).unwrap();
                super::menu::filter_picker(&rows, "openai-compatible", "kimi");
                if let Some(field) = picker.itemAtIndex(0).and_then(|i| i.view()).and_then(|v| v.subviews().firstObject()).and_then(|f| f.downcast::<objc2_app_kit::NSTextField>().ok()) {
                    field.setStringValue(&super::util::ns("kimi"));
                }
                let path = out.join("provider-openai-compatible-add-model-search-kimi.png");
                let ok = stack(&path, &picker, true, mtm);
                wrote(&path, ok, &mut failures);
            }
        }
        // A blank start (public): nothing connected, the engine off: Add Provider's six rows, Custom API's choice
        // of kind, and the built-in engine's submenu with its model's licence.
        let off = engine_states().into_iter().find(|(n, _)| *n == "off").map(|(_, e)| e).unwrap_or_default();
        let blank = crate::ApiProviders { unchosen: true, selected: None, models: vec![], catalog: vec![], ..crate::demo_all_providers(ProviderAuth::Missing) };
        let mut blank = blank;
        for p in &mut blank.providers { p.auth = ProviderAuth::Missing; }
        for (name, dark) in [("add-provider-blank", true), ("add-provider-blank-light", false)] {
            let (menu, _) = super::menu::api_providers_menu(&blank, &Default::default(), Some((&off, "low")), None, Some(true), None, mtm);
            let path = out.join(format!("{name}.png"));
            let ok = stack(&path, &menu, dark, mtm);
            wrote(&path, ok, &mut failures);
            if let Some(choice) = sub(&menu, super::provider_rows::CUSTOM_PROVIDER) {
                let path = out.join(format!("add-provider-custom-choice{}.png", if dark { "" } else { "-light" }));
                let ok = stack(&path, &choice, dark, mtm);
                wrote(&path, ok, &mut failures);
            }
            // The engine added (its model the agent's): its submenu under Providers, the licence under its model.
            let added = crate::ApiProviders { unchosen: false, ..blank.clone() };
            let (menu, _) = super::menu::api_providers_menu(&added, &Default::default(), Some((&off, "low")), None, Some(false), None, mtm);
            if let Some(engine_menu) = sub(&menu, super::menu::BUILT_IN_ENGINE) {
                let path = out.join(format!("provider-built-in-engine-licence{}.png", if dark { "" } else { "-light" }));
                let ok = stack(&path, &engine_menu, dark, mtm);
                wrote(&path, ok, &mut failures);
            }
        }
        // Connect for a custom provider: the URL field above the key (optional).
        for (name, provider, title, key_required, url, dark) in [
            ("panel-openai-compatible", "openai-compatible", "OpenAI-Compatible", false, "", true),
            ("panel-openai-compatible-light", "openai-compatible", "OpenAI-Compatible", false, "", false),
            ("panel-openai-compatible-change-url", "openai-compatible", "OpenAI-Compatible", false, "http://localhost:1234/v1", true),
            ("panel-anthropic-compatible", "anthropic-compatible", "Anthropic-Compatible", false, "", true),
            ("panel-anthropic-compatible-light", "anthropic-compatible", "Anthropic-Compatible", false, "", false),
        ] {
            let path = out.join(format!("{name}.png"));
            let controller = super::Controller::new(mtm);
            let mut panel = super::provider_key::KeyPanel::new(provider, title, Some(key_required), &controller, mtm);
            let look = if dark { unsafe { NSAppearanceNameDarkAqua } } else { unsafe { NSAppearanceNameAqua } };
            panel.panel.setAppearance(NSAppearance::appearanceNamed(look).as_deref());
            panel.reset(None);
            panel.set_url(url);
            let ok = super::agent_render::capture_window(&panel.panel, &path);
            wrote(&path, ok, &mut failures);
        }
    }
    // Anthropic's key panel: straight to the paste field (no sign-in), empty and with a key from the clipboard.
    for (name, clipboard) in [("provider-anthropic-key-panel", None), ("provider-anthropic-key-panel-prefilled", Some("sk-ant-api03-demo0123456789abcdefghijklmnopqrstuvwxyzABCDEFGH"))] {
        let path = dir.join(format!("{name}.png"));
        let controller = super::Controller::new(mtm);
        let mut panel = super::provider_key::KeyPanel::new("anthropic", "Anthropic", None, &controller, mtm);
        panel.panel.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
        panel.reset(clipboard);
        let ok = super::agent_render::capture_window(&panel.panel, &path);
        wrote(&path, ok, &mut failures);
    }
    // A real-length key (106 characters, like the user's Anthropic key) pasted into each panel, focused with the caret at
    // its end and unfocused, dark and light: the dots stay on one line inside the field and scroll; then a refusal
    // in Anthropic's own words (two lines, the panel one line taller).
    let long_key = format!("sk-ant-usr01-{}", "Q7mZx2Lr9TfA4kVb8NwC1yHs6PgD3jUe5oRi0".repeat(3))[..106].to_string();
    for (name, provider, title, dark, focused, status) in [
        ("provider-anthropic-key-panel-long-dark", "anthropic", "Anthropic", true, true, None),
        ("provider-anthropic-key-panel-long-light", "anthropic", "Anthropic", false, true, None),
        ("provider-anthropic-key-panel-long-unfocused-dark", "anthropic", "Anthropic", true, false, None),
        ("provider-anthropic-key-panel-long-unfocused-light", "anthropic", "Anthropic", false, false, None),
        ("provider-anthropic-key-panel-refused-dark", "anthropic", "Anthropic", true, false, Some("Anthropic: This organization's API access has been disabled. Contact your organization administrator to restore access.")),
        ("provider-anthropic-key-panel-refused-light", "anthropic", "Anthropic", false, false, Some("Anthropic: This organization's API access has been disabled. Contact your organization administrator to restore access.")),
        ("provider-key-panel-long-dark", "opencode-go", "OpenCode Go", true, true, None),
        ("provider-key-panel-long-light", "opencode-go", "OpenCode Go", false, true, None),
    ] {
        let path = dir.join(format!("{name}.png"));
        let controller = super::Controller::new(mtm);
        let mut panel = super::provider_key::KeyPanel::new(provider, title, None, &controller, mtm);
        let look = if dark { unsafe { NSAppearanceNameDarkAqua } } else { unsafe { NSAppearanceNameAqua } };
        panel.panel.setAppearance(NSAppearance::appearanceNamed(look).as_deref());
        panel.reset(Some(&long_key));
        if let Some(why) = status { panel.set_status(Some((why, true))); }
        // Focused, the field editor holds the text: it must scroll sideways inside the field, never grow downwards.
        let mut one_line = true;
        if focused {
            panel.panel.makeFirstResponder(Some(&panel.field));
            match panel.field.currentEditor() {
                Some(editor) => {
                    editor.setSelectedRange(objc2_foundation::NSRange::new(long_key.len(), 0));
                    let (frame, field) = (editor.frame(), panel.field.bounds());
                    one_line = editor.isHorizontallyResizable() && frame.size.height <= field.size.height;
                    eprintln!("{name}: field editor {:.0}×{:.0} in a {:.0}×{:.0} field, scrolls sideways: {}", frame.size.width, frame.size.height,
                        field.size.width, field.size.height, editor.isHorizontallyResizable());
                }
                None => one_line = false,
            }
        }
        let ok = super::agent_render::capture_window(&panel.panel, &path) && panel.key() == long_key && one_line;
        wrote(&path, ok, &mut failures);
    }
    // The macOS reference beside it: a stock password field as Interface Builder makes one (scrolling, no wrap) at
    // the same width, focused, with the same key.
    for (name, dark) in [("native-password-field-long-dark", true), ("native-password-field-long-light", false)] {
        use objc2::MainThreadOnly as _;
        let path = dir.join(format!("{name}.png"));
        let window = objc2_app_kit::NSPanel::initWithContentRect_styleMask_backing_defer(objc2_app_kit::NSPanel::alloc(mtm), rect(0.0, 0.0, super::provider_key::WIDTH, 64.0),
            objc2_app_kit::NSWindowStyleMask::Titled, objc2_app_kit::NSBackingStoreType::Buffered, false);
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&super::util::ns("Native password field"));
        let look = if dark { unsafe { NSAppearanceNameDarkAqua } } else { unsafe { NSAppearanceNameAqua } };
        window.setAppearance(NSAppearance::appearanceNamed(look).as_deref());
        let field = objc2_app_kit::NSSecureTextField::initWithFrame(objc2_app_kit::NSSecureTextField::alloc(mtm), rect(20.0, 20.0, super::provider_key::WIDTH - 40.0, 24.0));
        field.setFont(Some(&super::util::font(13.0)));
        if let Some(cell) = field.cell() { cell.setScrollable(true); cell.setWraps(false); }
        field.setStringValue(&super::util::ns(&long_key));
        window.contentView().unwrap().addSubview(&field);
        window.makeFirstResponder(Some(&field));
        if let Some(editor) = field.currentEditor() { editor.setSelectedRange(objc2_foundation::NSRange::new(long_key.len(), 0)); }
        let ok = super::agent_render::capture_window(&window, &path);
        wrote(&path, ok, &mut failures);
    }
    // The connect panel: sign-in starting, waiting with its code, failed; then the API-key fallback.
    for (name, code, status, key) in [
        ("provider-sign-in-starting", None, None, None),
        ("provider-sign-in-code", Some("DLXZ-HLMT"), None, None),
        ("provider-sign-in-failed", Some("DLXZ-HLMT"), Some(("The sign-in code expired", true)), None),
        ("provider-key-panel", None, None, Some(None)),
        ("provider-key-panel-prefilled", None, None, Some(Some("sk-demo0123456789abcdefghijklmnopqrstuv"))),
        ("provider-key-panel-checking", None, Some(("Checking…", false)), Some(Some("sk-demo0123456789abcdefghijklmnopqrstuv"))),
        ("provider-key-panel-rejected", None, Some(("OpenCode Go: access rejected / no subscription", true)), Some(Some("sk-demo0123456789abcdefghijklmnopqrstuv"))),
    ] {
        let path = dir.join(format!("{name}.png"));
        let controller = super::Controller::new(mtm);
        let mut panel = super::provider_key::KeyPanel::new("opencode-go", "OpenCode Go", None, &controller, mtm);
        panel.panel.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
        if let Some(code) = code { panel.show_code(code, "https://opencode.ai/console/device?user_code=DLXZ-HLMT"); }
        if let Some(clipboard) = key { panel.reset(clipboard); }
        if let Some(status) = status { panel.set_status(Some(status)); if key.is_some() { panel.set_busy(!status.1); } }
        let ok = super::agent_render::capture_window(&panel.panel, &path);
        wrote(&path, ok, &mut failures);
    }
    let engines = engine_states();
    for (name, engine) in &engines {
        for (listening, l) in [(true, "on"), (false, "off")] {
            for (appearance, dark) in [("dark", true), ("light", false)] {
                let path = dir.join(format!("menu-top-engine-{name}-listening-{l}-{appearance}.png"));
                let ok = tiles(&path, listening, all_on, engine.state.look(), true, dark, mtm);
                wrote(&path, ok, &mut failures);
            }
        }
        for (appearance, dark) in [("", true), ("-light", false)] {
            let path = dir.join(format!("menu-engine-{name}{appearance}.png"));
            let ok = stack(&path, &super::menu::engine_submenu(engine, Some("low"), None, mtm).0, dark, mtm);
            wrote(&path, ok, &mut failures);
        }
    }
    // Speaking ▸ Speed at the menu's real width: the quick default, natural speed and the fastest, dark and light.
    for (percent, appearance, dark) in [(115, "dark", true), (115, "light", false), (100, "dark", true), (160, "dark", true)] {
        let path = dir.join(format!("menu-speed-{percent}-{appearance}.png"));
        let row = super::controls::SpeedRow::new(percent, mtm);
        row.setFrame(rect(0.0, 0.0, MENU_WIDTH, super::controls::SPEED_ROW_HEIGHT));
        let ok = capture(&path, &row, dark);
        wrote(&path, ok, &mut failures);
    }
    // The sample waiting for Jarvis to load, and Jarvis unable to load, dark and light.
    for (name, note, dark) in [("loading-dark", "Loading Jarvis…", true), ("loading-light", "Loading Jarvis…", false), ("unavailable-dark", "Jarvis unavailable", true)] {
        let path = dir.join(format!("menu-speed-sample-{name}.png"));
        let row = super::controls::SpeedRow::new(115, mtm);
        row.setFrame(rect(0.0, 0.0, MENU_WIDTH, super::controls::SPEED_ROW_HEIGHT));
        row.set_note(Some(note));
        let ok = capture(&path, &row, dark);
        wrote(&path, ok, &mut failures);
    }
    // Seeing ▸ Recording on the user's 3456 × 2234 panel (default, and light), with a 4K display beside it, and with a
    // saved choice the menu no longer offers.
    let laptop = (3456, 2234);
    let default = crate::ScreenRecording::default();
    let saved = crate::ScreenRecording { resolution: crate::ScreenResolution::P1080, fps: 3 };
    for (name, screen, displays, dark) in [
        ("menu-recording", default, vec![laptop], true),
        ("menu-recording-light", default, vec![laptop], false),
        ("menu-recording-two-displays", default, vec![laptop, (3840, 2160)], true),
        ("menu-recording-saved-1080", saved, vec![laptop], true),
    ] {
        let path = dir.join(format!("{name}.png"));
        let ok = recording(&path, &screen, &displays, dark, mtm);
        wrote(&path, ok, &mut failures);
    }
    // The whole menu's first frame: the demo state (setup unfinished), healthy, every tile off, setup needed, and
    // a capture failure.
    use crate::{ListenState, Permission, PermissionState, RuntimeSettings, Settings, UiUpdate};
    let granted = |s: &mut super::State| {
        let all = Permission::ALL.iter().map(|p| (*p, PermissionState::Granted)).collect();
        s.apply(UiUpdate::Permissions(all));
    };
    let all_off = |s: &mut super::State| {
        s.apply(UiUpdate::Listen(ListenState::Paused));
        s.off_since = Some("09:22".into());
        s.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: true, seeing: false, ..Default::default() }));
        s.apply(UiUpdate::Settings(Settings { speak_replies: false, launch_at_login: true, ..Default::default() }));
        s.engine.state = EngineState::Off;
        s.engine.context_limit = None;
        s.engine.cache_used_bytes = None;
        s.engine.cache_limit_bytes = None;
    };
    type Setup = Box<dyn Fn(&mut super::State)>;
    let menus: Vec<(&str, bool, Setup)> = vec![
        ("menu", true, Box::new(|_| {})),
        ("menu-light", false, Box::new(|_| {})),
        ("menu-healthy", true, Box::new(granted)),
        ("menu-haiku-not-connected", true, Box::new(move |s| { granted(s); s.api = demo_api_providers(ProviderAuth::Missing, false); s.engine.state = EngineState::Off; })),
        ("menu-haiku-connected", true, Box::new(move |s| { granted(s); s.api = demo_api_providers(ProviderAuth::Connected, false); s.engine.state = EngineState::Off; })),
        ("menu-haiku-rejected", true, Box::new(move |s| { granted(s); s.api = demo_api_providers(ProviderAuth::Problem("OpenCode Go: access rejected / no subscription".into()), false); s.engine.state = EngineState::Off; })),
        // [providers] the user's live setup and every provider connected: Agent ▸ Model, no Inference Engine section.
        ("providers/agent-section", true, Box::new(move |s| { granted(s); s.api = crate::demo_all_providers(ProviderAuth::Connected); s.engine.state = EngineState::Off; })),
        ("providers/agent-section-light", false, Box::new(move |s| { granted(s); s.api = crate::demo_all_providers(ProviderAuth::Connected); s.engine.state = EngineState::Off; })),
        // Background Tasks: every provider connected, Voice Names off.
        ("menu-background-tasks", true, Box::new(move |s| { granted(s); s.api = crate::demo_all_providers(ProviderAuth::Connected); s.engine.state = EngineState::Off;
            s.settings_value.background_off = vec![crate::model::BackgroundTask::VoiceNames]; })),
        ("menu-background-tasks-light", false, Box::new(move |s| { granted(s); s.api = crate::demo_all_providers(ProviderAuth::Connected); s.engine.state = EngineState::Off; })),
        ("menu-tiles-off", true, Box::new(all_off)),
        ("menu-tiles-off-light", false, Box::new(all_off)),
        ("menu-setup-needed", true, Box::new(|s| s.apply(UiUpdate::Listen(ListenState::NeedsSetup)))),
        ("menu-failed", true, Box::new(|s| s.apply(UiUpdate::Listen(ListenState::Failed("audio capture stopped".into()))))),
        // A problem the user can act on, as the app words it: the whole sentence, wrapped (23 Oct).
        ("menu-problem", true, Box::new(move |s| {
            granted(s);
            s.apply(UiUpdate::Listen(ListenState::Listening));
            s.apply(UiUpdate::ComponentStatus { component: "archive".into(), message: Some("The disk is almost full, so Heimdall stopped recording. Free some space and it resumes.".into()) });
        })),
        // Two at once: both sentences whole, the row grows.
        ("menu-problems", true, Box::new(move |s| {
            granted(s);
            s.apply(UiUpdate::Listen(ListenState::Listening));
            s.apply(UiUpdate::ComponentStatus { component: "archive".into(), message: Some("The disk is almost full, so Heimdall stopped recording. Free some space and it resumes.".into()) });
            s.apply(UiUpdate::ComponentStatus { component: "capture".into(), message: Some("Heimdall can\u{2019}t open the microphone or app audio. Turn Listening off and on to retry.".into()) });
        })),
        ("menu-problem-light", false, Box::new(move |s| {
            granted(s);
            s.apply(UiUpdate::Listen(ListenState::Listening));
            s.apply(UiUpdate::ComponentStatus { component: "speech".into(), message: Some("Heimdall can\u{2019}t speak right now, so replies only appear on screen.".into()) });
        })),
    ];
    for (name, dark, setup) in menus {
        let path = dir.join(format!("{name}.png"));
        let ok = full_menu(&path, dark, mtm, setup);
        wrote(&path, ok, &mut failures);
    }
    failures.extend(checks(mtm));
    failures
}

/// The Recording submenu as the menu builds it: its real row views, stacked in order. Section headers and
/// separators are native menu items with no view; they are drawn here in the menu's header style and hairline,
/// so their exact metrics are an approximation of AppKit's.
fn recording(path: &Path, screen: &crate::ScreenRecording, displays: &[(u32, u32)], dark: bool, mtm: MainThreadMarker) -> bool {
    let (menu, _, _) = super::menu::recording_menu(screen, displays, false, None, mtm);
    stack(path, &menu, dark, mtm)
}

/// The engine in each state the menu shows, on its one model; Ready with the figures Splash reports.
pub(super) fn engine_states() -> Vec<(&'static str, EngineStatus)> {
    let model = EngineModel {
        id: "qwen3.8-27b".into(),
        label: "Qwen3.8 27B".into(),
        quant: "UD-Q4_K_M".into(),
        source: "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M".into(),
        ..Default::default()
    };
    let off = EngineStatus {
        state: EngineState::Off,
        model_id: model.id.clone(),
        model_label: model.label.clone(),
        models: vec![model],
        port: 8100,
        ..Default::default()
    };
    let ready = EngineStatus {
        state: EngineState::Ready,
        context_limit: Some(262_144),
        cache_used_bytes: Some(44_237_195_264),
        cache_limit_bytes: Some(100 << 30),
        memory_limit_bytes: Some(48 << 30),
        ..off.clone()
    };
    vec![
        ("off", off.clone()),
        ("starting", EngineStatus { state: EngineState::Starting, ..off.clone() }),
        ("ready", ready.clone()),
        ("idle", EngineStatus { state: EngineState::Idle, ..ready }),
        ("error", EngineStatus { state: EngineState::Error("Inference engine unavailable".into()), ..off }),
    ]
}

/// Colours and metrics of the real menu, sampled from its on-screen renders (lab/ui-renders/menu*.png, 2×): drawn
/// offscreen, AppKit's semantic colours come out without the menu's material (translucent text far too bright), so
/// the native rows drawn here use the sampled values.
struct MenuLook {
    title: f64,
    grey: f64,
    icon: f64,
    hairline: f64,
}
impl MenuLook {
    fn of(dark: bool) -> Self {
        if dark {
            Self { title: 250.0, grey: 126.0, icon: 220.0, hairline: 60.0 }
        } else {
            Self { title: 6.0, grey: 138.0, icon: 38.0, hairline: 225.0 }
        }
    }
}
fn grey(v: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(v / 255.0, v / 255.0, v / 255.0, 1.0)
}
// Item heights measured on the real menu: 24-pt rows, an 11.5-pt separator item (the hairline 5 pt down), a
// 23.5-pt section header (its text centred 13.5 pt down); 5 pt above the first item.
const ROW: f64 = 24.0;
const SEPARATOR: f64 = 11.5;
const HEADER: f64 = 23.5;

thread_local! {
    /// The native row `stacked` draws under the pointer (by title), for hover renders.
    pub(crate) static HOVER: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// A submenu's items stacked in order, drawn without a menu (6 pt above and below).
pub(crate) fn stack(path: &Path, menu: &objc2_app_kit::NSMenu, dark: bool, mtm: MainThreadMarker) -> bool {
    let items: Vec<_> = menu.itemArray().iter().filter(|i| !i.isHidden()).collect();
    let font = super::util::menu_font();
    let text = items.iter().filter(|i| i.view().is_none() && !i.isSeparatorItem()).map(|i| super::util::text_width(&i.title().to_string(), &font) + 39.0 + 32.0);
    let width = items.iter().filter_map(|i| i.view()).filter(|v| v.downcast_ref::<LabelRow>().is_none()).map(|v| v.frame().size.width).chain(text).fold(200.0, f64::max)
        // AppKit's own width for the menu as built: badges (Memory ▸'s sizes) widen it, which the titles alone miss.
        .max(menu.size().width).ceil();
    let container = stacked(menu, width, (6.0, 6.0), dark, mtm);
    capture(path, &container, dark)
}

/// `menu`'s visible items laid out top to bottom at `width`: view rows are the menu's own views (taken out of their
/// items); separators, section headers and native rows (symbol, title, chevron, key equivalent; grey when disabled)
/// are drawn here in the real menu's metrics and sampled colours.
pub(crate) fn stacked(menu: &objc2_app_kit::NSMenu, width: f64, pad: (f64, f64), dark: bool, mtm: MainThreadMarker) -> Retained<NSView> {
    use objc2::MainThreadOnly as _;
    use objc2_app_kit::{NSBox, NSBoxType, NSFont, NSFontWeightSemibold, NSImageView, NSTextField, NSTitlePosition};
    let look = MenuLook::of(dark);
    let items: Vec<_> = menu.itemArray().iter().filter(|i| !i.isHidden()).collect();
    let item_height = |i: &objc2_app_kit::NSMenuItem| match i.view() {
        Some(v) => v.frame().size.height,
        None if i.isSeparatorItem() => SEPARATOR,
        None if i.isSectionHeader() => HEADER,
        None if super::menu::subtitle_of(i).is_some() => ROW + super::menu::subtitle_extra(),
        None => ROW,
    };
    let height = pad.0 + pad.1 + items.iter().map(|i| item_height(i)).sum::<f64>();
    let container = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, width, height));
    // An open section's panel behind its header and rows (`menu_group`), as the live menu's row layers draw it.
    let mut run: Option<f64> = None;
    let mut top = pad.0;
    for item in items.iter() {
        let h = item_height(item);
        if let Some(p) = super::menu_group::place(item) {
            if p.first { run = Some(top); }
            if let Some(from) = run.filter(|_| p.last) {
                run = None;
                let (white, alpha) = super::menu_group::fill(dark);
                let inset = super::menu_group::INSET;
                let panel = NSBox::initWithFrame(NSBox::alloc(mtm), rect(inset, height - top - h, width - 2.0 * inset, top + h - from));
                panel.setBoxType(NSBoxType::Custom);
                panel.setTitlePosition(NSTitlePosition::NoTitle);
                panel.setBorderWidth(0.0);
                panel.setCornerRadius(super::menu_group::RADIUS);
                panel.setFillColor(&NSColor::colorWithSRGBRed_green_blue_alpha(white, white, white, alpha));
                container.addSubview(&panel);
            }
        }
        top += h;
    }
    let label = |text: &str, font: &NSFont, colour: &NSColor, x: f64, centre: f64, right: bool| {
        let field = NSTextField::labelWithString(&super::util::ns(text), mtm);
        field.setFont(Some(font));
        field.setTextColor(Some(colour));
        // A label's text sits 2 pt inside its frame; 4 pt more width keeps the last glyph whole.
        let size = field.intrinsicContentSize();
        let w = size.width.ceil() + 4.0;
        let x = if right { x - w + 2.0 } else { x - 2.0 };
        field.setFrame(rect(x, (height - centre - size.height / 2.0).round(), w, size.height.ceil()));
        container.addSubview(&field);
        field
    };
    let symbol = |image: &objc2_app_kit::NSImage, size: f64, weight: f64, colour: &NSColor, x: f64, centre: f64| {
        use objc2_app_kit::NSImageSymbolConfiguration;
        let config = NSImageSymbolConfiguration::configurationWithPointSize_weight(size, weight).configurationByApplyingConfiguration(
            &NSImageSymbolConfiguration::configurationWithPaletteColors(&objc2_foundation::NSArray::from_retained_slice(&[colour.retain()])),
        );
        let image = image.imageWithSymbolConfiguration(&config).unwrap_or_else(|| image.retain());
        let s = image.size();
        let view = NSImageView::imageViewWithImage(&image, mtm);
        // A provider's template mark (not a symbol) takes the icon colour the way the menu tints it.
        if image.isTemplate() { view.setContentTintColor(Some(colour)); }
        view.setFrame(rect((x - s.width / 2.0).round(), (height - centre - s.height / 2.0).round(), s.width, s.height));
        container.addSubview(&view);
    };
    use objc2::Message as _;
    let font = super::util::menu_font();
    let mut top = pad.0;
    for item in items.iter() {
        let h = item_height(item);
        let centre = top + h / 2.0;
        // A row with a second line: the title in the upper part, the line under it in grey (AppKit's subtitle).
        let subtitle = item.view().is_none().then(|| super::menu::subtitle_of(item)).flatten();
        let title_centre = if subtitle.is_some() { top + ROW / 2.0 + 1.0 } else { centre };
        if let Some(view) = item.view() {
            item.setView(None);
            container.addSubview(&view);
            view.setFrame(rect(0.0, height - top - h, width, h));
        } else if item.isSeparatorItem() {
            let line = NSBox::initWithFrame(NSBox::alloc(mtm), rect(16.0, height - top - 6.0, width - 32.0, 1.0));
            line.setBoxType(NSBoxType::Custom);
            line.setTitlePosition(NSTitlePosition::NoTitle);
            line.setBorderWidth(0.0);
            line.setFillColor(&grey(look.hairline));
            container.addSubview(&line);
        } else if item.isSectionHeader() {
            let bold = unsafe { NSFont::systemFontOfSize_weight(font.pointSize() - 2.0, NSFontWeightSemibold) };
            label(&item.title().to_string(), &bold, &grey(look.grey), 16.0, top + 13.5, false);
        } else {
            let enabled = item.isEnabled();
            // The pointer's highlight on this native row (renders only): the accent rounded rectangle, white ink.
            let hovered = enabled && HOVER.with(|h| h.borrow().as_deref() == Some(super::menu::row_title(item).as_str()));
            if hovered {
                let fill = NSBox::initWithFrame(NSBox::alloc(mtm), rect(5.0, height - top - h, width - 10.0, h));
                fill.setBoxType(NSBoxType::Custom);
                fill.setTitlePosition(NSTitlePosition::NoTitle);
                fill.setBorderWidth(0.0);
                fill.setCornerRadius(6.0);
                fill.setFillColor(&NSColor::controlAccentColor());
                container.addSubview(&fill);
            }
            let ink = if hovered { NSColor::whiteColor() } else { grey(if enabled { look.title } else { look.grey }) };
            if let Some(image) = item.image() {
                let icon = if hovered { NSColor::whiteColor() } else { grey(if enabled { look.icon } else { look.grey }) };
                symbol(&image, (font.pointSize() * 0.9).round(), unsafe { objc2_app_kit::NSFontWeightRegular }, &icon, 22.25, centre);
            }
            let field = label(&item.title().to_string(), &font, &ink, 39.0, title_centre, false);
            if let Some(line) = &subtitle {
                let small = NSFont::systemFontOfSize(font.pointSize() - 2.0);
                let ink = if hovered { NSColor::whiteColor() } else { grey(look.grey) };
                label(line, &small, &ink, 39.0, title_centre + super::menu::subtitle_extra(), false);
            }
            if let Some(attributed) = item.attributedTitle() {
                // Runs in the secondary colour draw in the grey sampled from the real menu (offscreen, the dynamic
                // colour comes out near white, as `capture` notes for `LabelRow`).
                let copy = objc2_foundation::NSMutableAttributedString::from_attributed_nsstring(&attributed);
                let secondary = NSColor::secondaryLabelColor();
                let (len, mut at) = (attributed.length(), 0usize);
                while at < len {
                    let mut range = objc2_foundation::NSRange::new(0, 0);
                    let colour = unsafe { attributed.attribute_atIndex_effectiveRange(objc2_app_kit::NSForegroundColorAttributeName, at, &mut range) };
                    if !hovered && colour.and_then(|c| c.downcast::<NSColor>().ok()).is_some_and(|c| objc2_foundation::NSObjectProtocol::isEqual(&*c, Some(&*secondary))) {
                        unsafe { copy.addAttribute_value_range(objc2_app_kit::NSForegroundColorAttributeName, &grey(look.grey), range) };
                    }
                    at = range.location + range.length.max(1);
                }
                field.setAttributedStringValue(&copy);
                // A title with a value at its tab stop (`menu::set_value`) takes the row's width up to the arrow column.
                if attributed.string().to_string().contains('\t') {
                    let mut f = field.frame();
                    f.size.width = width - f.origin.x - 30.0;
                    field.setFrame(f);
                }
            }
            if item.hasSubmenu() {
                if let Some(chevron) = super::util::symbol("chevron.right") {
                    symbol(&chevron, (font.pointSize() * 0.8).round(), unsafe { objc2_app_kit::NSFontWeightSemibold }, &ink, width - 16.0 - 3.0, centre);
                }
            }
            if item.state() == objc2_app_kit::NSControlStateValueOn {
                if let Some(check)=super::util::symbol("checkmark") {symbol(&check,10.0,0.0,&ink,8.0,centre);}
            }
            let key = item.keyEquivalent().to_string();
            if !key.is_empty() {
                // The modifiers as the real menu draws them, ⌃⌥⇧⌘ in that order.
                let mask = item.keyEquivalentModifierMask();
                let mods: String = [(objc2_app_kit::NSEventModifierFlags::Control, '⌃'), (objc2_app_kit::NSEventModifierFlags::Option, '⌥'), (objc2_app_kit::NSEventModifierFlags::Shift, '⇧'), (objc2_app_kit::NSEventModifierFlags::Command, '⌘')]
                    .into_iter()
                    .filter(|(f, _)| mask.contains(*f))
                    .map(|(_, g)| g)
                    .collect();
                label(&format!("{mods}{}", key.to_uppercase()), &font, &grey(look.grey), width - 16.0, centre, true);
            }
            // A badge (Memory's sizes) sits at the trailing edge, before a submenu's chevron, in the grey AppKit draws.
            if let Some(badge) = item.badge().and_then(|b| b.stringValue()) {
                let right = if item.hasSubmenu() { width - 34.0 } else { width - 16.0 };
                label(&badge.to_string(), &font, &grey(look.grey), right, centre, true);
            }
        }
        top += h;
    }
    container
}

/// The whole menu as it first opens, built by the real `menu::build` on a UI state with no status item (nothing
/// reaches the menu bar), in the demo's state: listening with setup unfinished, the agent running since 09:14,
/// Speaking and Launch at Login on, the engine ready. `setup` changes that state first.
pub(crate) fn full_menu(path: &Path, dark: bool, mtm: MainThreadMarker, setup: impl FnOnce(&mut super::State)) -> bool {
    let state = demo_menu(mtm, setup);
    let container = stacked(&state.menu, MENU_WIDTH, (5.0, 0.5), dark, mtm);
    capture_rounded(path, &container, dark)
}

/// Memory ▸ and the whole menu with its Memory row, dark and light, from `memory_demo`: nothing appears on screen.
/// Returns the paths that failed.
pub(crate) fn memory(dir: &Path, mtm: MainThreadMarker) -> Vec<String> {
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    for (appearance, dark) in [("dark", true), ("light", false)] {
        let sub = dir.join(format!("menu-memory-{appearance}.png"));
        if !stack(&sub, &super::menu::memory_item(&memory_demo(), mtm).submenu().unwrap(), dark, mtm) { failures.push(sub.display().to_string()) }
        let whole = dir.join(format!("menu-with-memory-{appearance}.png"));
        if !full_menu(&whole, dark, mtm, |_| {}) { failures.push(whole.display().to_string()) }
    }
    for path in failures.iter() { eprintln!("render {path} FAILED") }
    failures
}

/// The user's Memory submenu on 3 Nov 18:35: Drama 3 loaded, Heimdall's own process, the search index, speakers, the
/// agent's runtime and the islands.
pub(crate) fn memory_demo() -> crate::memory::MemoryReport {
    use crate::memory::{MemoryPart, MemoryReport, CORE_LABEL};
    const MB: u64 = 1 << 20;
    let parts = vec![
        MemoryPart { label: "Drama 3".into(), icon: "text.bubble", bytes: 23245 * MB },
        MemoryPart { label: CORE_LABEL.into(), icon: "waveform", bytes: 4608 * MB },
        MemoryPart { label: "Search Index".into(), icon: "magnifyingglass", bytes: 2150 * MB },
        MemoryPart { label: "Speakers".into(), icon: "person.2", bytes: 582 * MB },
        MemoryPart { label: "Agent".into(), icon: "terminal", bytes: 88 * MB },
        MemoryPart { label: "Islands".into(), icon: "rectangle.on.rectangle", bytes: 58 * MB },
    ];
    MemoryReport { total: parts.iter().map(|p| p.bytes).sum(), parts }
}

/// The UI state `full_menu` draws, with its menu built (also the live left-opening check's menu).
pub(crate) fn demo_menu(mtm: MainThreadMarker, setup: impl FnOnce(&mut super::State)) -> super::State {
    use crate::{AgentState, Harness, ListenState, RuntimeSettings, SessionState, Settings, Source, SourceKind, UiUpdate};
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let options = crate::Options { fake_permissions: true, ..Default::default() };
    let mut state = super::State::new(mtm, controller, options, Box::new(|_| {}), None, menu, None);
    state.apply(UiUpdate::Listen(ListenState::Listening));
    state.apply(UiUpdate::Sources(vec![Source { id: "mic".into(), kind: SourceKind::Mic, name: "MacBook Pro Microphone".into(), enabled: true }]));
    state.apply(UiUpdate::Agent(AgentState { harness: Harness::Pi, session: SessionState::Running { since: "09:14".into() } }));
    state.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: false, ..Default::default() }));
    state.apply(UiUpdate::Settings(Settings { speak_replies: true, launch_at_login: true, ..Default::default() }));
    let ready = engine_states().into_iter().find(|(n, _)| *n == "ready").unwrap().1;
    state.engine = ready;
    state.memory_demo = Some(memory_demo());
    state.decider = super::setup::demo_decider();
    setup(&mut state);
    state.rebuild_menu();
    state
}

/// Shortcuts ▸ as the real `menu::shortcuts_menu` builds it, on a UI state with no status item: listening, Seeing
/// off, the islands shown.
pub(crate) fn shortcuts_submenu(path: &Path, dark: bool, mtm: MainThreadMarker) -> bool {
    use crate::{ListenState, RuntimeSettings, Settings, UiUpdate};
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let options = crate::Options { fake_permissions: true, ..Default::default() };
    let mut state = super::State::new(mtm, controller, options, Box::new(|_| {}), None, menu, None);
    state.apply(UiUpdate::Listen(ListenState::Listening));
    state.apply(UiUpdate::RuntimeSettings(RuntimeSettings { dry_run: false, seeing: false, ..Default::default() }));
    state.apply(UiUpdate::Settings(Settings::default()));
    let sub = super::menu::shortcuts_preview(&mut state, mtm);
    stack(path, &sub, dark, mtm)
}

/// Friends (personal edition), for the renders: the Friends submenu as the menu builds it, with two friends (one
/// revoked), and the invite panel for `qr_png` (a QR code made by `friends::invite::qr_png`). Writes
/// `friends-submenu.png` and `friend-invite.png` into `dir`; returns the paths that failed.
pub(crate) fn friends(dir: &Path, qr_png: &[u8], mtm: MainThreadMarker) -> Vec<String> {
    use crate::{FriendRow, UiUpdate};
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let controller = super::Controller::new(mtm);
    // The controller as the menu's target, as `State::target` gives it.
    let target: objc2::rc::Retained<objc2::runtime::AnyObject> =
        objc2::rc::Retained::into_super(objc2::rc::Retained::into_super(controller.clone()));
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let options = crate::Options { fake_permissions: true, friends: true, ..Default::default() };
    let mut state = super::State::new(mtm, controller, options, Box::new(|_| {}), None, menu, None);
    state.apply(UiUpdate::Friends(vec![
        FriendRow { id: "render-foster".into(), label: "Morgan".into(), revoked: false },
        FriendRow { id: "render-pat".into(), label: "Pat".into(), revoked: true },
    ]));
    let (sub, _) = super::menu::friends_menu(&state.friends, Some(&*target), mtm);
    let submenu = dir.join("friends-submenu.png");
    if !stack(&submenu, &sub, true, mtm) {
        failures.push(submenu.display().to_string());
    }
    let invite = dir.join("friend-invite.png");
    let mut panel = super::friend_invite::InvitePanel::default();
    panel.hidden = true;
    let shown = panel.show("Morgan", qr_png, &target, mtm);
    if !(shown && panel.capture(&invite, true)) {
        failures.push(invite.display().to_string());
    }
    panel.close();
    failures
}

/// Through the real Objective-C selectors, no menu: an unavailable tile reports itself disabled to accessibility and
/// refuses AXPress (so it never reaches the controller); an available one is enabled; the menu-bar images are
/// template images of the glyph's size.
fn checks(mtm: MainThreadMarker) -> Vec<String> {
    use objc2::msg_send;
    use objc2::runtime::Bool;
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failures.push(name.to_string());
        }
    };
    let seeing = TileView::new(1, TileLook::On, false, mtm);
    let enabled: bool = unsafe { msg_send![&*seeing, isAccessibilityEnabled] };
    let pressed: Bool = unsafe { msg_send![&*seeing, accessibilityPerformPress] };
    check("unavailable tile: accessibility disabled, AXPress refused", !enabled && !pressed.as_bool() && seeing.is_on());
    seeing.set_enabled(true);
    let enabled: bool = unsafe { msg_send![&*seeing, isAccessibilityEnabled] };
    check("available tile: accessibility enabled", enabled);
    for off in [false, true] {
        let image = helmet::menu_bar_image(off);
        check(&format!("menu-bar image off={off}: template, {:?}", helmet::GLYPH), image.isTemplate() && image.size() == helmet::GLYPH);
    }
    let red = helmet::menu_bar_attention_image();
    check("menu-bar attention image: not a template, same size", !red.isTemplate() && red.size() == helmet::GLYPH);
    failures.extend(api_checks(mtm));
    failures.extend(api_switch_checks(mtm));
    failures.extend(speed_checks(mtm));
    failures.extend(health_menu_checks(mtm));
    failures.extend(shortcut_checks(mtm));
    failures.extend(personalise_checks(mtm));
    failures
}

/// Personalisation (26 Oct 16:45) through the real views and selectors on a UI state with no status item: the section
/// replaced Speaking and ends with Name & Icon ▸, the mark as its image, and nothing named Personalise or Wake Words
/// remains; a mark press sends `SetIcon` and moves the selection in place (the same
/// cells, the menu not rebuilt); Return in Name sends `SetName` and the name reaches Quit, the tooltip and VoiceOver;
/// an invalid name sends nothing and says why; the default identity keeps "Quit Heimdall"; Personalisation reads
/// header, caption, Model ▸ (as Agent does), then Speed, Wake Sound and Name & Icon ▸. The identity is the default again afterwards.
pub(crate) fn personalise_checks(mtm: MainThreadMarker) -> Vec<String> {
    use super::personalise::{self, Field};
    use crate::identity::{Icon, Identity, IdentityAction};
    use crate::{ListenState, UiAction};
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok {
            failures.push(name.to_string());
        }
    };
    crate::identity::set(Identity::default());
    let emitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<UiAction>::new()));
    let recorded = emitted.clone();
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let mut state = super::State::new(mtm, controller, crate::Options { fake_permissions: true, ..Default::default() }, Box::new(move |a| recorded.borrow_mut().push(a)), None, menu, None);
    state.apply(crate::UiUpdate::Listen(ListenState::Listening));
    state.rebuild_menu();
    // The clicks act in the open menu (a closed one is rebuilt as it opens, not synced).
    state.menu_open = true;
    let previous = super::STATE.with(|s| s.replace(Some(state)));
    let last = || emitted.borrow().last().cloned();
    let titles = || super::with_state_now(|s| s.menu.itemArray().iter().map(|i| i.title().to_string()).collect::<Vec<_>>()).unwrap();
    let t = titles();
    let quit = |t: &[String]| t.iter().any(|x| x == "Quit Heimdall");
    check("the default identity keeps \"Quit Heimdall\" and the tooltip \"Heimdall · listening\"", quit(&t)
        && super::status_tooltip(&crate::identity::name(), &ListenState::Listening, false) == "Heimdall · listening"
        && helmet::menu_bar_image(false).accessibilityDescription().is_some_and(|d| d.to_string() == "Heimdall"));
    let at = |title: &str| t.iter().position(|x| x == title);
    check("Name & Icon has a submenu and the mark as its image",
        at(personalise::TITLE).is_some()
            && super::with_state_now(|s| personalise::item(&s.personalise_rows).is_some_and(|i| i.hasSubmenu() && i.image().is_some_and(|m| m.isTemplate()))).unwrap());
    // Personalisation reads as the Agent and Background Tasks sections do (header, caption, Model ▸), then Speed, Wake
    // Sound and Name & Icon ▸ before the separator; no Speaking section and no Personalise row are left.
    let section = super::with_state_now(|s| {
        let items = s.menu.itemArray();
        // Its header is a foldable section's row (30 Oct), titled as the section.
        let i = items.iter().position(|i| i.title().to_string() == super::menu::PERSONALISATION_TITLE)?;
        let at = |k: usize| items.objectAtIndex(i + k);
        // Climb 2: the voice's row is Voice, its value after a tab (`menu::row_title`).
        Some((at(1).view().is_some(), super::menu::row_title(&at(2)), at(2).hasSubmenu(), at(3).view().is_some(), at(4).title().to_string(),
            at(5).title().to_string(), at(5).hasSubmenu(), at(6).isSeparatorItem()))
    }).flatten();
    check(&format!("Personalisation: header, caption, Voice ▸, Speed, Wake Sound, Name & Icon ▸ ({section:?})"), section == Some((true, super::speaking_menu::VOICE_ROW_TITLE.to_string(), true, true,
        super::menu::WAKE_SOUND_TITLE.to_string(), personalise::TITLE.to_string(), true, true)));
    let headers = super::with_state_now(|s| s.menu.itemArray().iter().filter(|i| i.isSectionHeader()).map(|i| i.title().to_string()).collect::<Vec<_>>()).unwrap();
    check(&format!("no Speaking section, no Personalise or Voice row ({headers:?})"), !headers.iter().any(|h| h == "Speaking")
        && !t.iter().any(|x| x == "Voice" || x == "Personalise" || x == "Wake Words"));

    let cells = || super::with_state_now(|s| personalise::cells(&s.personalise_rows)).unwrap();
    let before = cells();
    let items_before = super::with_state_now(|s| s.menu.itemArray().iter().map(|i| Retained::as_ptr(&i) as usize).collect::<Vec<_>>()).unwrap();
    let selected = || cells().into_iter().filter(|(_, c)| c.is_selected()).map(|(i, _)| i).collect::<Vec<_>>();
    check("the grid: the helmet first and selected, then the bundled marks", before.first().is_some_and(|(i, c)| *i == Icon::Helmet && c.is_selected())
        && before.len() == 1 + crate::identity::BUNDLED.len() && selected() == [Icon::Helmet]);
    let leaf = Icon::Symbol { name: "leaf".into() };
    let leaf_cell = before.iter().find(|(i, _)| *i == leaf).map(|(_, c)| c.clone());
    let pressed: objc2::runtime::Bool = leaf_cell.as_ref().map_or(objc2::runtime::Bool::NO, |c| unsafe { objc2::msg_send![&**c, accessibilityPerformPress] });
    let same_cells = cells().iter().zip(&before).all(|((_, a), (_, b))| Retained::as_ptr(a) == Retained::as_ptr(b));
    let items_after = super::with_state_now(|s| s.menu.itemArray().iter().map(|i| Retained::as_ptr(&i) as usize).collect::<Vec<_>>()).unwrap();
    check("pressing the leaf sends SetIcon and moves the selection in place (same cells, same menu)", pressed.as_bool()
        && last() == Some(UiAction::Identity(IdentityAction::SetIcon(leaf.clone()))) && selected() == [leaf.clone()] && same_cells && items_before == items_after
        && crate::identity::current().icon == leaf);
    let count = emitted.borrow().len();
    if let Some(c) = &leaf_cell { let _: objc2::runtime::Bool = unsafe { objc2::msg_send![&**c, accessibilityPerformPress] }; }
    check("pressing the selected mark again sends nothing", emitted.borrow().len() == count);

    let name_field = super::with_state_now(|s| personalise::input(&s.personalise_rows, Field::Name)).flatten();
    check("the Name field shows the name", name_field.as_ref().is_some_and(|f| f.stringValue().to_string() == "Heimdall" && f.isEditable()));
    if let Some(f) = &name_field {
        f.setStringValue(&super::util::ns("Ivy"));
        personalise::press_return(f);
    }
    let t = titles();
    check("Return in Name sends SetName(\"Ivy\")", last() == Some(UiAction::Identity(IdentityAction::SetName("Ivy".into()))) && crate::identity::name() == "Ivy");
    check("the name reaches Quit Ivy, the tooltip and VoiceOver", t.iter().any(|x| x == "Quit Ivy") && !quit(&t)
        && super::status_tooltip(&crate::identity::name(), &ListenState::Listening, false) == "Ivy · listening"
        && helmet::menu_bar_image(true).accessibilityDescription().is_some_and(|d| d.to_string() == "Ivy, listening off"));
    let count = emitted.borrow().len();
    if let Some(f) = &name_field {
        f.setStringValue(&super::util::ns("Ivy!"));
        personalise::press_return(f);
    }
    let error = super::with_state_now(|s| personalise::error_text(&s.personalise_rows, Field::Name)).flatten();
    check(&format!("an invalid name sends nothing and says why ({error:?})"), emitted.borrow().len() == count && crate::identity::name() == "Ivy"
        && error.as_deref().is_some_and(|e| e.contains("Only letters")));
    if let Some(f) = &name_field {
        f.setStringValue(&super::util::ns("Ivy"));
        personalise::press_return(f);
    }
    let error = super::with_state_now(|s| personalise::error_text(&s.personalise_rows, Field::Name)).flatten();
    check("a valid value again clears the reason and sends nothing new", error.is_none() && emitted.borrow().len() == count);
    let user = super::with_state_now(|s| personalise::input(&s.personalise_rows, Field::UserName)).flatten();
    if let Some(f) = &user {
        f.setStringValue(&super::util::ns("Evelyn"));
        personalise::press_return(f);
    }
    check("Return in Your Name sends SetUserName(\"Evelyn\")", last() == Some(UiAction::Identity(IdentityAction::SetUserName("Evelyn".into()))));
    // The app's answer (its saved identity) changes nothing more; a different one moves the selection back.
    super::with_state_now(|s| s.apply(crate::UiUpdate::Identity(Identity::default())));
    let t = titles();
    check("the app's identity moves the selection and the names back in place", selected() == [Icon::Helmet] && quit(&t)
        && name_field.as_ref().is_some_and(|f| f.stringValue().to_string() == "Heimdall"));
    crate::identity::set(Identity::default());
    super::STATE.with(|s| *s.borrow_mut() = previous);
    failures
}

/// Shortcuts ▸ (24 Oct): the real menu has the submenu, each item carries its shortcut's key equivalent with ⌃⌘, its
/// id as tag and the controller's `runShortcut:`, and the switches' checks follow the state; the user's own Heimdall holds
/// the four keys, so the Carbon round trip (register, take back while the menu is open, register again) runs on
/// ⌃⌥⇧⌘F19, which nothing uses. That one key is registered globally for the check (and left unregistered); this
/// process never installs Heimdall's own keys. No status item, nothing on screen.
fn shortcut_checks(mtm: MainThreadMarker) -> Vec<String> {
    use crate::shortcuts::Shortcut;
    use crate::{ListenState, Options, RuntimeSettings, UiUpdate};
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_string()); }
    };
    let menu = objc2_app_kit::NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let actions = std::rc::Rc::new(std::cell::RefCell::new(vec![]));
    let sink = actions.clone();
    let mut state = super::State::new(mtm, super::Controller::new(mtm), Options { fake_permissions: true, ..Default::default() }, Box::new(move |a| sink.borrow_mut().push(a)), None, menu, None);
    state.apply(UiUpdate::Listen(ListenState::Listening));
    state.apply(UiUpdate::RuntimeSettings(RuntimeSettings { seeing: true, ..Default::default() }));
    state.rebuild_menu();
    let sub = state.menu.itemArray().iter().find(|i| i.title().to_string() == super::menu::SHORTCUTS_TITLE).and_then(|i| i.submenu());
    check("menu: a Shortcuts submenu", sub.is_some());
    let Some(sub) = sub else { return failures };
    let items: Vec<_> = sub.itemArray().iter().collect();
    check("Shortcuts: one item per shortcut", items.len() == Shortcut::ALL.len());
    let mask = objc2_app_kit::NSEventModifierFlags::Control | objc2_app_kit::NSEventModifierFlags::Command;
    for (item, s) in items.iter().zip(Shortcut::ALL) {
        let k = s.keys();
        let ok = item.title().to_string() == s.title()
            && item.keyEquivalent().to_string() == k.key.to_string()
            && item.keyEquivalentModifierMask() == mask
            && item.tag() == s.id() as isize
            && item.action() == Some(objc2::sel!(runShortcut:))
            && item.image().is_some();
        check(&format!("Shortcuts: {} is {} and runs its shortcut", s.title(), k.label()), ok);
    }
    let responds: bool = unsafe { objc2::msg_send![&*state.controller, respondsToSelector: objc2::sel!(runShortcut:)] };
    check("the controller answers runShortcut:", responds);
    let checked = |state: &super::State, s: Shortcut| super::menu::shortcut_on(state, s);
    check("Shortcuts: no column of checks (the icons show the state)", items.iter().all(|i| i.state() != objc2_app_kit::NSControlStateValueOn));
    check("Shortcuts: Listening and Seeing on", checked(&state, Shortcut::Listening) && checked(&state, Shortcut::Seeing));
    // ⌃⌘/ runs the Seeing tile's own toggle: the same action, the check follows at once.
    state.toggle_tile(crate::Tile::Seeing);
    check("Seeing's toggle: SetSeeing(false) and its icon slashed", actions.borrow().last() == Some(&crate::UiAction::SetSeeing(false)) && !checked(&state, Shortcut::Seeing));
    state.toggle_tile(crate::Tile::Listening);
    check("Listening's toggle: SetListening(false) and its icon slashed", actions.borrow().last() == Some(&crate::UiAction::SetListening(false)) && !checked(&state, Shortcut::Listening));
    // The Carbon round trip on a key nothing uses.
    const PROBE: u32 = 99;
    let probe = crate::shortcuts::Keys { code: 80, key: 'x', control: true, option: true, shift: true, command: true };
    let registered = super::draw::hotkey::register(PROBE, probe.code as u32, probe.carbon_modifiers(), mtm).is_ok() && super::draw::hotkey::registered().contains(&PROBE);
    check("Carbon: ⌃⌥⇧⌘F19 registers", registered);
    super::draw::hotkey::suspend();
    check("Carbon: the menu open takes it back", !super::draw::hotkey::registered().contains(&PROBE));
    let failed = super::draw::hotkey::resume();
    check("Carbon: the menu closed registers it again", failed.is_empty() && super::draw::hotkey::registered().contains(&PROBE));
    super::draw::hotkey::suspend();
    failures
}

/// The island's health pill, Open Menu: a microphone that arrives while the menu is closed (its rows only synced in
/// place) is in the Microphone submenu once the click is handled. No status item, so nothing opens or tracks on screen.
fn health_menu_checks(mtm: MainThreadMarker) -> Vec<String> {
    use crate::island::HealthControl;
    use crate::{Microphone, Microphones, Options, UiUpdate};
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_string()); }
    };
    let mic = |id: &str, name: &str| Microphone { id: id.into(), name: name.into(), connected: true, built_in: id == "builtin" };
    let menu = objc2_app_kit::NSMenu::new(mtm);
    menu.setAutoenablesItems(false);
    let mut state = super::State::new(mtm, super::Controller::new(mtm), Options { fake_permissions: true, ..Default::default() }, Box::new(|_| {}), None, menu, None);
    state.apply(UiUpdate::Microphones(Microphones { devices: vec![mic("builtin", "MacBook Pro Microphone")], primary: None, in_use: Some("builtin".into()), extras: vec![] }));
    state.rebuild_menu();
    let listed = |state: &super::State, name: &str| state.microphone_buttons.iter().any(|(b, _, _)| b.title().to_string().contains(name));
    state.apply(UiUpdate::Microphones(Microphones {
        devices: vec![mic("builtin", "MacBook Pro Microphone"), mic("usb", "USB Microphone")],
        primary: None,
        in_use: Some("builtin".into()),
        extras: vec![],
    }));
    check("health menu: a microphone that arrives while closed is not listed until the menu opens", !listed(&state, "USB Microphone"));
    state.health_clicked("health".into(), HealthControl::OpenMenu);
    check("health menu: Open Menu rebuilds the menu before it opens (the new microphone is listed)", listed(&state, "USB Microphone"));
    failures
}

/// Speaking ▸ Speed through the real selectors on the row the real menu build makes: a slider to accessibility,
/// AXIncrement/AXDecrement step 5% and emit `SetVoiceSpeed` (the app then plays the sample), the ends clamp, and
/// a settings update moves the knob.
fn speed_checks(mtm: MainThreadMarker) -> Vec<String> {
    use crate::{Settings, UiAction, UiUpdate};
    use objc2::msg_send;
    use objc2::runtime::Bool;
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_string()); }
    };
    let emitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<UiAction>::new()));
    let recorded = emitted.clone();
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let mut state = super::State::new(mtm, super::Controller::new(mtm), crate::Options { fake_permissions: true, ..Default::default() }, Box::new(move |a| recorded.borrow_mut().push(a)), None, menu, None);
    state.rebuild_menu();
    let previous = super::STATE.with(|s| s.replace(Some(state)));
    let row = super::with_state_now(|s| s.speed_row.clone()).flatten();
    check("the menu has a Speaking ▸ Speed row", row.is_some());
    if let Some(row) = row {
        let role: Retained<objc2_foundation::NSString> = unsafe { msg_send![&*row, accessibilityRole] };
        check("Speed is a slider to accessibility", role.to_string() == "AXSlider");
        check("Speed starts at the quick default", row.percent() == crate::DEFAULT_VOICE_SPEED_PERCENT);
        let up: Bool = unsafe { msg_send![&*row, accessibilityPerformIncrement] };
        check("AXIncrement steps to 1.2x and asks for it (sample follows)", up.as_bool() && row.percent() == 120 && emitted.borrow().last() == Some(&UiAction::SetVoiceSpeed(120)));
        let _: Bool = unsafe { msg_send![&*row, accessibilityPerformDecrement] };
        let _: Bool = unsafe { msg_send![&*row, accessibilityPerformDecrement] };
        check("AXDecrement steps down", row.percent() == 110 && emitted.borrow().last() == Some(&UiAction::SetVoiceSpeed(110)));
        let saved = super::with_state_now(|s| s.settings_value.voice_speed_percent);
        check("the released value is the menu's setting until the app answers", saved == Some(110));
        for _ in 0..20 { let _: Bool = unsafe { msg_send![&*row, accessibilityPerformIncrement] }; }
        check("the fastest end clamps at 1.6x", row.percent() == 160);
        let description: Retained<objc2_foundation::NSString> = unsafe { msg_send![&*row, accessibilityValueDescription] };
        check("VoiceOver reads 1.6×", description.to_string() == "1.6×");
        super::with_state(|s| s.apply(UiUpdate::Settings(Settings { voice_speed_percent: 135, ..s.settings_value.clone() })));
        super::with_state(|s| super::menu::sync_tracking(s));
        let shown = super::with_state_now(|s| s.speed_row.as_ref().map(|r| r.percent())).flatten();
        check("a settings update moves the knob", shown == Some(135));
    }
    super::STATE.with(|s| *s.borrow_mut() = previous);
    failures
}

/// The real tile block, as the menu builds it, drawn on the menu's background at 2×. `line`: with the line under
/// the tiles, as the first frame shows it (Listening off says since when; on and healthy it says nothing).
#[allow(clippy::too_many_arguments)]
pub(crate) fn tiles(path: &Path, listening: bool, saved: [bool; 3], engine: TileLook, line: bool, dark: bool, mtm: MainThreadMarker) -> bool {
    let look = |on: bool| if on { TileLook::On } else { TileLook::Off };
    let looks = [look(listening), look(saved[0]), look(saved[1]), look(saved[2]), engine];
    let views: Vec<Retained<TileView>> = crate::Tile::ALL
        .iter()
        .enumerate()
        .map(|(i, tile)| TileView::new(i, looks[i], tile.available(listening), mtm))
        .collect();
    let row = TileRow::new(&views, mtm);
    let tiles_height = row.frame().size.height;
    row.setFrameSize(NSSize::new(MENU_WIDTH, tiles_height));
    let status = (line && !listening).then(|| {
        let status = LabelRow::new(16.0, mtm);
        status.set("Listening off since 14:05", false, false);
        status
    });
    let Some(status) = status else { return capture(path, &row, dark) };
    let status_height = status.frame().size.height;
    let height = tiles_height + status_height;
    let container = {
        use objc2::MainThreadOnly as _;
        NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, MENU_WIDTH, height))
    };
    row.setFrameOrigin(objc2_foundation::NSPoint::new(0.0, status_height));
    status.setFrame(rect(0.0, 0.0, MENU_WIDTH, status_height));
    container.addSubview(&row);
    container.addSubview(&status);
    capture(path, &container, dark)
}

/// `view` drawn through `cacheDisplayInRect` at 2× over the menu's own background colour. Grey `LabelRow` lines
/// take the grey sampled from the real menu (its session line): drawn offscreen, NSTextField renders translucent
/// text far brighter than the menu does (white at 55% came out at 209 of 255 on the dark background; the real
/// menu shows 124).
pub(crate) fn capture(path: &Path, view: &NSView, dark: bool) -> bool {
    capture_with(path, view, dark, 0.0)
}

/// As `capture`, the background a menu window's rounded rectangle (11-pt corners, sampled), transparent outside.
pub(crate) fn capture_rounded(path: &Path, view: &NSView, dark: bool) -> bool {
    capture_with(path, view, dark, 11.0)
}

pub(super) fn capture_with(path: &Path, view: &NSView, dark: bool, corner: f64) -> bool {
    let name = unsafe { if dark { NSAppearanceNameDarkAqua } else { NSAppearanceNameAqua } };
    view.setAppearance(NSAppearance::appearanceNamed(name).as_deref());
    let grey = if dark { 124.0 / 255.0 } else { 128.0 / 255.0 };
    for row in label_rows(view) {
        row.render_grey(&NSColor::colorWithSRGBRed_green_blue_alpha(grey, grey, grey, 1.0));
    }
    let bounds = view.bounds();
    let scale = 2.0;
    let Some(rep) = (unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(), std::ptr::null_mut(), (bounds.size.width * scale) as isize,
            (bounds.size.height * scale) as isize, 8, 4, true, false, objc2_app_kit::NSDeviceRGBColorSpace, 0, 0,
        )
    }) else {
        return false;
    };
    rep.setSize(bounds.size);
    view.cacheDisplayInRect_toBitmapImageRep(bounds, &rep);
    // The menu's own background colour, sampled from the real menu renders.
    let background = if dark { 28.0 / 255.0 } else { 251.0 / 255.0 };
    helmet::png(path, bounds.size.width, bounds.size.height, scale, || {
        NSColor::colorWithSRGBRed_green_blue_alpha(background, background, background, 1.0).setFill();
        objc2_app_kit::NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(bounds, corner, corner).fill();
        // Over the background (an image rep alone draws as a copy, alpha and all).
        let image = objc2_app_kit::NSImage::initWithSize(objc2_app_kit::NSImage::alloc(), bounds.size);
        image.addRepresentation(&rep);
        image.drawInRect(bounds);
    })
}

/// Every `LabelRow` in `view`'s tree, `view` included.
fn label_rows(view: &NSView) -> Vec<Retained<LabelRow>> {
    use objc2::Message as _;
    let mut out: Vec<Retained<LabelRow>> = view.retain().downcast::<LabelRow>().into_iter().collect();
    for sub in view.subviews().iter() {
        out.extend(label_rows(&sub));
    }
    out
}

/// API Providers through the real Objective-C selectors and buttons, in process (no pointer, keyboard, window on
/// screen, browser or clipboard): radio rows, the picker's checkboxes, the local radio, Connect, the key panel.
fn api_checks(mtm: MainThreadMarker) -> Vec<String> {
    use crate::{demo_api_providers, ProviderAuth, UiAction, UiUpdate};
    use objc2::msg_send;
    use objc2_app_kit::{NSControlStateValueOff, NSControlStateValueOn};
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_string()); }
    };
    // The builder: one selection, the reason on the row, unusable picker rows disabled, the provider's mark.
    for (auth, local) in [(ProviderAuth::Missing, false), (ProviderAuth::Connected, false), (ProviderAuth::Connected, true)] {
        let api = demo_api_providers(auth.clone(), local);
        let (menu, rows) = super::menu::api_providers_menu(&api, &Default::default(), None, None, None, None, mtm);
        let (haiku, ..) = &rows.models[0];
        check(&format!("{auth:?} local={local}: Haiku radio on only when selected"), (haiku.state() == NSControlStateValueOn) == !local);
        let title = haiku.attributedTitle().string().to_string();
        check(&format!("{auth:?} local={local}: the row says why it cannot run"), title.contains("Connect OpenCode Go") == (auth == ProviderAuth::Missing));
        check(&format!("{auth:?} local={local}: a selected row stays enabled; an unusable unselected one is disabled"),
            haiku.isEnabled() == (!local || auth != ProviderAuth::Missing));
        let provider = menu.itemArray().iter().find(|i| i.title().to_string().contains("OpenCode Go")).unwrap();
        check(&format!("{auth:?}: provider row has its template mark"), provider.image().is_some_and(|i| i.isTemplate()));
        check(&format!("{auth:?}: in Add Provider until a key exists, then the provider's submenu"),
            (auth == ProviderAuth::Missing) == (provider.action() == Some(objc2::sel!(connectProvider:)) && provider.title().to_string() == "OpenCode Go subscription…" && provider.submenu().is_none()));
        // [providers] A provider's picker exists once it is connected (provider, then Add Model).
        if auth != ProviderAuth::Missing {
            let gemini = rows.catalog.iter().find(|(_, _, id)| id == "gemini-4-argon-contributor").unwrap();
            check("an unusable picker row cannot be added", !gemini.0.isEnabled());
            check("the picker checks the added model", rows.catalog.iter().find(|(_, _, id)| id == "claude-haiku-5-5").unwrap().0.state() == NSControlStateValueOn);
        } else {
            check("no picker before the provider is connected", rows.catalog.is_empty());
        }
        // "right now the Claude Haiku 5.5 it is always uncollapsed which is just stupid".
        let (disclosure, details) = &rows.details[0];
        check(&format!("{auth:?} local={local}: the model's details start collapsed"), disclosure.as_ref().is_some_and(|d| d.state() == NSControlStateValueOff) && details.iter().all(|i| i.isHidden()));
        check(&format!("{auth:?} local={local}: the slider has a stop per step from 128K to 1M"), rows.sliders[0].as_ref().is_some_and(|(s, f, steps)| s.numberOfTickMarks() == 8 && s.integerValue() == 0 && f.stringValue().to_string() == "Context 128K" && steps.len() == 8));
    }
    // An expanded row shows its slider and price; the provider without a sign-in says Replace Key….
    let open: std::collections::BTreeSet<(String, String)> = [("anthropic".to_string(), "claude-opus-5-5".to_string())].into();
    let api = crate::demo_anthropic_providers(ProviderAuth::Connected);
    let (menu, rows) = super::menu::api_providers_menu(&api, &open, None, None, None, None, mtm);
    check("an expanded row shows its details", rows.details[0].1.iter().all(|i| !i.isHidden()) && rows.details[1].1.iter().all(|i| i.isHidden()));
    let price = menu.itemArray().iter().filter(|i| !i.isHidden()).filter_map(|i| i.view()).filter_map(|v| v.downcast::<LabelRow>().ok()).any(|r| r.text() == "$4 in · $20 out per M tokens");
    check("Opus's price shows in its details", price);
    let quota = menu.itemArray().iter().filter_map(|i| i.view()).filter_map(|v| v.downcast::<LabelRow>().ok()).any(|r| r.text().contains("left"));
    check("no spend estimate under Anthropic (no quota)", !quota && rows.pickers.iter().any(|(p, _)| p == "anthropic"));
    let own = menu.itemArray().iter().find(|i| i.title().to_string() == "Anthropic API").and_then(|i| i.submenu()).unwrap();
    let titles: Vec<String> = own.itemArray().iter().map(|i| i.title().to_string()).collect();
    check("Anthropic's submenu: Add Model, Replace Key…, Disconnect; no credits", titles.contains(&"Add Model".to_string()) && titles.contains(&"Replace Key…".to_string()) && titles.contains(&"Disconnect".to_string())
        && !titles.iter().any(|t| t.contains("Max 5x") || t.contains("Credits")) && !titles.contains(&"Sign In Again…".to_string()));
    let anthropic = menu.itemArray().iter().find(|i| i.title().to_string() == "Anthropic API").unwrap();
    check("Anthropic's row has its template mark", anthropic.image().is_some_and(|i| i.isTemplate()));
    // [providers] Every provider in the sub-table: a picker per connected provider with a search over its list, ChatGPT
    // a provider like the others, Local holding the built-in engine's models and the own engine.
    let engine = engine_states().into_iter().find(|(n, _)| *n == "ready").map(|(_, e)| e).unwrap();
    let all = crate::demo_all_providers(ProviderAuth::Connected);
    let (menu, rows) = super::menu::api_providers_menu(&all, &Default::default(), Some((&engine, "low")), None, None, None, mtm);
    let titles: Vec<String> = menu.itemArray().iter().map(|i| i.title().to_string()).collect();
    check("providers: Codex subscription, Local Splash Engine, Anthropic API, OpenCode Go subscription and the two custom APIs", ["Codex subscription", super::menu::BUILT_IN_ENGINE, "Anthropic API", "OpenCode Go subscription", "OpenAI-Compatible API", "Anthropic-Compatible API"].iter().all(|t| titles.contains(&t.to_string()))
        && !titles.iter().any(|t| t == "ChatGPT" || t == "Anthropic" || t == "OpenCode Go" || t == "Local" || t.contains("Coming Soon") || t == "OpenRouter" || t == "Own Engine"));
    // [providers] Subscriptions first, then this Mac, then the API keys.
    let at = |t: &str| titles.iter().position(|x| x == t).unwrap_or(usize::MAX);
    check("Providers in order: subscriptions, this Mac, API keys", at("Codex subscription").max(at("OpenCode Go subscription")) < at(super::menu::BUILT_IN_ENGINE)
        && at(super::menu::BUILT_IN_ENGINE) < at("Anthropic API") && at("Anthropic API") < at("OpenAI-Compatible API") && at("OpenAI-Compatible API") < at("Anthropic-Compatible API"));
    let chatgpt = menu.itemArray().iter().find(|i| i.title().to_string() == "Codex subscription").and_then(|i| i.submenu()).unwrap();
    let chatgpt_titles: Vec<String> = chatgpt.itemArray().iter().map(|i| i.title().to_string()).collect();
    check("ChatGPT's submenu: Add Model, Sign In Again…, Disconnect", ["Add Model", "Sign In Again…", "Disconnect"].iter().all(|t| chatgpt_titles.contains(&t.to_string())));
    check("ChatGPT's mark is a template image", super::menu::provider_image("codex").is_some_and(|i| i.isTemplate()));
    // Add Provider: exactly the six rows in the user's order (both editions); Custom provider first asks which kind.
    let off = engine_states().into_iter().find(|(n, _)| *n == "off").map(|(_, e)| e).unwrap();
    let mut blank = crate::ApiProviders { unchosen: true, selected: None, models: vec![], catalog: vec![], ..crate::demo_all_providers(ProviderAuth::Missing) };
    for p in &mut blank.providers { p.auth = ProviderAuth::Missing; }
    let groups = super::provider_rows::add_groups(&blank, Some(&off), None, mtm);
    let offered: Vec<String> = groups.iter().flat_map(|(_, e)| e.iter().map(|e| e.title.trim_end_matches('\u{2026}').to_string())).collect();
    check(&format!("Add Provider: the six rows in order {offered:?}"), offered == super::provider_rows::add_order() && groups.len() == 1 && groups[0].0.is_empty());
    let custom = groups[0].1.iter().find(|e| e.title == super::provider_rows::CUSTOM_PROVIDER).and_then(|e| e.submenu.clone());
    let kinds: Vec<String> = custom.map(|m| m.itemArray().iter().map(|i| i.title().to_string()).collect()).unwrap_or_default();
    check(&format!("Custom API offers OpenAI-Compatible API… and Anthropic-Compatible API… {kinds:?}"), kinds == ["OpenAI-Compatible API…", "Anthropic-Compatible API…"]);
    let t3 = groups[0].1.iter().find(|e| e.title == super::provider_rows::CLAUDE_VIA_T3);
    check("Claude Code subscription routes to claudeViaT3:", t3.is_some_and(|e| e.action == objc2::sel!(claudeViaT3:)));
    // Connected providers and an engine in use leave the list; the relay stays while it is off, and leaves once on.
    let all_on = super::provider_rows::add_groups(&crate::demo_all_providers(ProviderAuth::Connected), Some(&engine), None, mtm);
    let left: Vec<String> = all_on.iter().flat_map(|(_, e)| e.iter().map(|e| e.title.clone())).collect();
    check(&format!("Add Provider keeps only what is not added {left:?}"), left == [super::provider_rows::CLAUDE_VIA_T3]);
    let relay_on = crate::ApiProviders { relay_on: true, ..crate::demo_all_providers(ProviderAuth::Connected) };
    let left = super::provider_rows::add_groups(&relay_on, Some(&engine), None, mtm);
    check("the relay on: Add Provider has nothing left to offer", left.iter().all(|(_, e)| e.is_empty()));
    check("no model chosen: the Agent line asks for a provider", super::menu::agent_model_problem(&blank).as_deref() == Some("Add a subscription in Model"));
    let (_, blank_rows) = super::menu::api_providers_menu(&blank, &Default::default(), Some((&off, "low")), None, None, None, mtm);
    check("no model chosen: the engine is not checked", blank_rows.local.as_ref().is_some_and(|b| b.state() == NSControlStateValueOff));
    // [providers] Native width: the list is no wider than its rows, not its pickers' long names.
    let view_width = menu.itemArray().iter().filter(|i| !i.isHidden()).filter_map(|i| i.view()).filter(|v| v.downcast_ref::<LabelRow>().is_none()).map(|v| v.frame().size.width).fold(0.0, f64::max);
    // Its widest row here is "Claude Haiku 5.5 · OpenCode Go subscription" with its disclosure; the pickers' long names (OpenRouter's
    // "Anthropic: Claude Opus 5.5") and the thinking selector no longer widen it.
    let widest = super::util::text_width("Claude Haiku 5.5 · OpenCode Go subscription", &super::util::menu_font()) + 52.0 + 20.0 + 30.0;
    check(&format!("Agent ▸ Model is as wide as its widest row ({view_width:.0} pt)"), (view_width - widest.ceil()).abs() <= 1.0);
    check("each connected provider has its own picker", ["anthropic", "opencode-go", "openai-compatible", "anthropic-compatible"].iter().all(|p| rows.pickers.iter().any(|(q, _)| q == p)));
    check("search keeps matching rows only", super::menu::filter_picker(&rows, "openai-compatible", "kimi") == 1 && super::menu::filter_picker(&rows, "openai-compatible", "") == 7 && super::menu::filter_picker(&rows, "openai-compatible", "x-ai grok") == 1);
    check("the built-in engine is a Model row, off while Opus is selected", rows.local.as_ref().is_some_and(|b| b.state() == NSControlStateValueOff) && rows.engine.len() == 1);
    let own = menu.itemArray().iter().find(|i| i.title().to_string() == "OpenAI-Compatible API").and_then(|i| i.submenu()).unwrap();
    let own_titles: Vec<String> = own.itemArray().iter().map(|i| i.title().to_string()).collect();
    check("a custom provider names its URL and offers Change URL…", own_titles.contains(&"openrouter.ai".to_string()) && own_titles.contains(&"Change URL…".to_string()));
    let mut built_in = all.clone();
    built_in.selected = None;
    let (_, rows) = super::menu::api_providers_menu(&built_in, &Default::default(), Some((&engine, "low")), None, None, None, mtm);
    check("the built-in engine's row is on when no API model is selected", rows.local.as_ref().is_some_and(|b| b.state() == NSControlStateValueOn) && rows.models.iter().all(|(b, ..)| b.state() == NSControlStateValueOff));

    // A failed sign-in shows under Connect…, in orange, also after the panel is gone.
    let mut failed = demo_api_providers(ProviderAuth::Missing, false);
    failed.providers[0].notice = Some("Sign-in failed: The sign-in code expired".into());
    let (_, rows) = super::menu::api_providers_menu(&failed, &Default::default(), None, None, None, None, mtm);
    let (menu, _) = super::menu::api_providers_menu(&failed, &Default::default(), None, None, None, None, mtm);
    let notice = menu.itemArray().iter().filter_map(|i| i.view()).filter_map(|v| v.downcast::<LabelRow>().ok()).any(|r| r.text() == "Sign-in failed: The sign-in code expired");
    check("a failed sign-in shows on the provider rows", notice && rows.models.len() == 1);
    check("the Agent line names the failed sign-in", super::menu::agent_model_problem(&failed).as_deref() == Some("Sign-in failed: The sign-in code expired"));

    let emitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<UiAction>::new()));
    let recorded = emitted.clone();
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let mut state = super::State::new(mtm, controller.clone(), crate::Options { fake_permissions: true, ..Default::default() }, Box::new(move |a| recorded.borrow_mut().push(a)), None, menu, None);
    state.engine = engine_states().into_iter().find(|(n, _)| *n == "ready").unwrap().1;
    state.api = demo_api_providers(ProviderAuth::Connected, true);
    state.rebuild_menu();
    let previous = super::STATE.with(|s| s.replace(Some(state)));
    let last = || emitted.borrow().last().cloned();
    // A closed menu neither rebuilds nor syncs on a state change (no 15–40 ms rebuild on the island's thread; night
    // 1027, no sync either): it is marked stale and rebuilt as it opens. Open, a change syncs its rows in place.
    let items = || super::with_state_now(|s| s.menu.itemArray().iter().map(|i| Retained::as_ptr(&i) as usize).collect::<Vec<_>>()).unwrap();
    let bug_title = || super::with_state_now(|s| s.menu.itemArray().iter().find(|i| i.action() == Some(objc2::sel!(openBugNotes:))).map(|i| i.title().to_string())).flatten();
    let before = items();
    let resting = bug_title();
    super::with_state_now(|s| s.apply(crate::UiUpdate::BugNotes(crate::demo_bug_notes())));
    let stale = super::with_state_now(|s| s.menu_stale).unwrap();
    check("a closed menu neither rebuilds nor syncs on a state change, and is marked for the next open", items() == before && bug_title() == resting && stale);
    super::with_state_now(|s| { s.rebuild_menu(); s.menu_open = true; });
    let opened = items();
    super::with_state_now(|s| s.apply(crate::UiUpdate::BugNotes(vec![])));
    super::with_state_now(|s| s.apply(crate::UiUpdate::BugNotes(crate::demo_bug_notes())));
    check("an open menu syncs its rows in place on a state change, without a rebuild", items() == opened
        && bug_title() == (!cfg!(feature = "public")).then(|| crate::model::bug_notes_title(crate::demo_bug_notes().iter().filter(|n| n.is_open()).count())));
    // The clicks below act in the open menu.
    // [providers] Agent ▸ Model: the built-in engine is a row of the Model list; no Inference Engine section.
    let built_in = || super::with_state_now(|s| (super::menu::row_title(s.api_item.as_ref().unwrap()), s.api_rows.local.as_ref().unwrap().state() == NSControlStateValueOn, s.engine_item.is_none())).unwrap();
    let no_engine_section = super::with_state_now(|s| s.menu.itemArray().iter().all(|i| i.title().to_string() != "Inference Engine")).unwrap();
    check("with the local model, Agent ▸ Model's built-in row is on, and no Inference Engine section", built_in() == (super::menu::MODEL_ROW_TITLE.to_string(), true, true) && no_engine_section);
    let haiku = super::with_state_now(|s| s.api_rows.models[0].0.clone()).unwrap();
    unsafe { haiku.performClick(None) };
    check("with Haiku, the built-in row turns off", built_in() == (super::menu::MODEL_ROW_TITLE.to_string(), false, true));
    check("clicking Haiku emits its selection", last() == Some(UiAction::SelectApiModel { provider: "opencode-go".into(), id: "claude-haiku-5-5".into() }));
    // This Mac's one row proposes the agent on the local engine (Confirm sends SelectLocalAgent; `--model-choice`).
    check("This Mac's row is the engine itself, on chooseLocalEngine:", super::with_state_now(|s| s.api_rows.local.as_ref().is_some_and(|b| b.action() == Some(objc2::sel!(chooseLocalEngine:)))).unwrap());
    // Without a key Haiku's row refuses selection.
    let before = emitted.borrow().len();
    super::with_state_now(|s| { s.apply(UiUpdate::ApiProviders(demo_api_providers(ProviderAuth::Missing, true))); s.rebuild_menu(); });
    let haiku = super::with_state_now(|s| s.api_rows.models[0].0.clone()).unwrap();
    unsafe { let _: () = msg_send![&*controller, chooseApiModel: &*haiku]; }
    check("an unconnected model refuses selection", emitted.borrow().len() == before);
    // The picker: unchecking removes, checking adds; an unusable model is refused.
    super::with_state_now(|s| { s.apply(UiUpdate::ApiProviders(demo_api_providers(ProviderAuth::Connected, false))); s.rebuild_menu(); });
    let row = |id: &str| super::with_state_now(|s| s.api_rows.catalog.iter().find(|(_, _, i)| i == id).unwrap().0.clone()).unwrap();
    unsafe { row("kimi-k3").performClick(None) };
    check("checking Kimi K3 adds it", last() == Some(UiAction::SetApiModelAdded { provider: "opencode-go".into(), id: "kimi-k3".into(), on: true }));
    unsafe { row("claude-haiku-5-5").performClick(None) };
    check("unchecking Haiku removes it", last() == Some(UiAction::SetApiModelAdded { provider: "opencode-go".into(), id: "claude-haiku-5-5".into(), on: false }));
    let before = emitted.borrow().len();
    let gemini = row("gemini-4-argon-contributor");
    unsafe { let _: () = msg_send![&*controller, toggleApiModel: &*gemini]; }
    check("an unusable model is never added", emitted.borrow().len() == before);
    // Connect: the panel opens in sign-in mode and asks the app to start; the code arrives and shows (no browser
    // offscreen); Cancel stops the sign-in. Use API Key… stops it too and switches to the key field.
    super::with_state_now(|s| { s.apply(UiUpdate::ApiProviders(demo_api_providers(ProviderAuth::Missing, false))); s.rebuild_menu(); });
    let connect = super::util::menu_item("Connect OpenCode Go…", None, None, "", None, mtm);
    connect.setTag(0);
    unsafe { let _: () = msg_send![&*controller, connectProvider: &*connect]; }
    check("Connect starts the browser sign-in", last() == Some(UiAction::StartProviderLogin { provider: "opencode-go".into() }));
    let views = super::with_state_now(|s| s.key_panel.as_ref().map(|p| (p.field.clone(), p.connect.clone(), p.status.clone(), p.panel.clone(), p.code.clone(), p.cancel.clone(), p.use_key.clone(), p.console.clone()))).flatten();
    let Some((field, button, status, window, code, cancel, use_key, console)) = views else { super::STATE.with(|s| *s.borrow_mut() = previous); drop(check); return failures };
    check("the panel waits in sign-in mode with no code yet", field.isHidden() && !code.isHidden() && code.stringValue().to_string() == "…");
    super::with_state_now(|s| s.apply(UiUpdate::ProviderLogin { provider: "opencode-go".into(), code: "ABCD-EFGH".into(), url: "https://opencode.ai/console/device?user_code=ABCD-EFGH".into() }));
    check("the code to approve shows in the panel", code.stringValue().to_string() == "ABCD-EFGH" && super::with_state_now(|s| s.key_panel.as_ref().unwrap().url.is_some()).unwrap());
    window.orderFront(None);
    window.setAlphaValue(0.0);
    unsafe { cancel.performClick(None) };
    check("Cancel stops the sign-in and closes the panel", last() == Some(UiAction::CancelProviderLogin { provider: "opencode-go".into() }) && !window.isVisible());
    unsafe { let _: () = msg_send![&*controller, connectProvider: &*connect]; }
    super::with_state_now(|s| s.apply(UiUpdate::ProviderConnected { provider: "opencode-go".into(), result: Err("The sign-in code expired".into()) }));
    check("a failed sign-in says why and stays open", status.stringValue().to_string() == "The sign-in code expired");
    check("the panel stays visible while the browser is in front", !window.hidesOnDeactivate());
    let reopen = super::with_state_now(|s| s.key_panel.as_ref().map(|p| p.reopen.clone())).flatten().unwrap();
    check("after a failure the browser button is Try Again", reopen.title().to_string() == "Try Again" && reopen.isEnabled());
    unsafe { reopen.performClick(None) };
    check("Try Again starts a new sign-in, not the dead link", last() == Some(UiAction::StartProviderLogin { provider: "opencode-go".into() }) && reopen.title().to_string() == "Open Browser");
    super::with_state_now(|s| s.apply(UiUpdate::ProviderConnected { provider: "opencode-go".into(), result: Err("The sign-in code expired".into()) }));
    unsafe { use_key.performClick(None) };
    check("Use API Key… stops the sign-in and shows an empty key field", last() == Some(UiAction::CancelProviderLogin { provider: "opencode-go".into() }) && !field.isHidden() && code.isHidden() && field.stringValue().to_string().is_empty());
    let before = emitted.borrow().len();
    unsafe { button.performClick(None) };
    check("an empty key leaves Connect disabled and sends nothing", emitted.borrow().len() == before && !button.isEnabled());
    field.setStringValue(&super::util::ns("sk-lab-0123456789abcdefghij"));
    // As typing does: the field's change notification enables Connect.
    unsafe { objc2_foundation::NSNotificationCenter::defaultCenter().postNotificationName_object(objc2_app_kit::NSControlTextDidChangeNotification, Some(AsRef::<objc2::runtime::AnyObject>::as_ref(&*field))) };
    check("typing a key enables Connect", button.isEnabled());
    unsafe { button.performClick(None) };
    check("Connect sends the pasted key and waits", last() == Some(UiAction::ConnectProviderKey { provider: "opencode-go".into(), key: "sk-lab-0123456789abcdefghij".into() }) && !button.isEnabled() && status.stringValue().to_string() == "Checking…");
    super::with_state_now(|s| s.apply(UiUpdate::ProviderConnected { provider: "opencode-go".into(), result: Err("OpenCode Go: access rejected / no subscription".into()) }));
    check("a rejected key keeps the panel open with the reason", button.isEnabled() && status.stringValue().to_string().contains("access rejected"));
    window.orderFront(None);
    super::with_state_now(|s| s.apply(UiUpdate::ProviderConnected { provider: "opencode-go".into(), result: Ok(()) }));
    check("a verified key closes the panel", !window.isVisible());
    // Prefill: only a key-shaped clipboard value.
    super::with_state_now(|s| s.key_panel.as_mut().unwrap().reset(Some("  sk-clip0123456789abcdefghijkl \n")));
    check("a key on the clipboard pre-fills the field", field.stringValue().to_string() == "sk-clip0123456789abcdefghijkl");
    super::with_state_now(|s| s.key_panel.as_mut().unwrap().reset(Some("meeting notes from today")));
    check("other clipboard text never fills the field", field.stringValue().to_string().is_empty());
    window.orderFront(None);
    let before = emitted.borrow().len();
    unsafe { console.performClick(None) };
    check("Open Console emits nothing and keeps the panel (offscreen opens no browser)", emitted.borrow().len() == before && window.isVisible());
    unsafe { cancel.performClick(None) };
    check("Cancel in key mode closes without sending a key", emitted.borrow().len() == before && !window.isVisible());
    let disconnect = super::util::menu_item("Disconnect", None, None, "", None, mtm);
    disconnect.setTag(0);
    unsafe { let _: () = msg_send![&*controller, disconnectProvider: &*disconnect]; }
    check("Disconnect asks the app to forget the key", last() == Some(UiAction::DisconnectProvider { provider: "opencode-go".into() }));
    // Anthropic: the disclosure opens the row's details in place, the slider sends its stop when let go, the credit
    // choice goes to the app, and Connect opens the paste field without a sign-in.
    super::with_state_now(|s| { s.apply(UiUpdate::ApiProviders(crate::demo_anthropic_providers(ProviderAuth::Connected))); s.rebuild_menu(); });
    let (disclosure, items) = super::with_state_now(|s| (s.api_rows.details[0].0.clone().unwrap(), s.api_rows.details[0].1.clone())).unwrap();
    unsafe { disclosure.performClick(None) };
    check("the disclosure shows Opus's details", items.iter().all(|i| !i.isHidden()) && super::with_state_now(|s| s.api_expanded.contains(&("anthropic".into(), "claude-opus-5-5".into()))).unwrap());
    let radio = super::with_state_now(|s| s.api_rows.models[0].0.clone()).unwrap();
    let before = emitted.borrow().len();
    unsafe { disclosure.performClick(None) };
    check("a second click collapses them again without selecting", items.iter().all(|i| i.isHidden()) && emitted.borrow().len() == before && radio.state() == NSControlStateValueOn);
    let (slider, field) = super::with_state_now(|s| s.api_rows.sliders[0].as_ref().map(|(sl, f, _)| (sl.clone(), f.clone()))).flatten().unwrap();
    slider.setIntegerValue(3);
    unsafe { let _: () = msg_send![&*controller, apiContextSlider: &*slider]; }
    check("the slider's stop goes to the app and the label follows", last() == Some(UiAction::SetApiModelContext { provider: "anthropic".into(), id: "claude-opus-5-5".into(), tokens: 501_714 }) && field.stringValue().to_string() == "Context 502K");
    let before = emitted.borrow().len();
    unsafe { let _: () = msg_send![&*controller, apiContextSlider: &*slider]; }
    check("the same stop again sends nothing", emitted.borrow().len() == before);
    // Thinking: Opus offers Low to Max (its thinking cannot be off), Sonnet Off to Max; Low is selected; a segment
    // the pointer lands on is the control itself; choosing a level tells the app once.
    let thinking = |i: usize| super::with_state_now(|s| s.api_rows.thinking[i].clone()).flatten().unwrap();
    let ((opus, opus_levels), (sonnet, sonnet_levels)) = (thinking(0), thinking(1));
    let titles = |c: &objc2_app_kit::NSSegmentedControl| (0..c.segmentCount()).map(|i| c.labelForSegment(i).map(|l| l.to_string()).unwrap_or_default()).collect::<Vec<_>>();
    check("Opus's thinking: Low, Medium, High, Max, at Low", titles(&opus) == ["Low", "Medium", "High", "Max"] && opus.selectedSegment() == 0 && opus_levels == ["low", "medium", "high", "max"]);
    check("Sonnet's thinking: Off to Max, at Low", titles(&sonnet) == ["Off", "Low", "Medium", "High", "Max"] && sonnet.selectedSegment() == 1 && sonnet_levels.len() == 5);
    let hit = unsafe { opus.superview() }.and_then(|host| {
        let f = opus.frame();
        let at = host.convertPoint_toView(objc2_foundation::NSPoint::new(f.origin.x + f.size.width * 0.6, f.origin.y + f.size.height / 2.0), unsafe { host.superview() }.as_deref());
        host.hitTest(at)
    });
    check("the pointer on the thinking selector reaches it", hit.is_some_and(|v| v.isDescendantOf(&opus)));
    opus.setSelectedSegment(2);
    unsafe { let _: () = msg_send![&*controller, apiThinking: &*opus]; }
    check("choosing High on Opus tells the app", last() == Some(UiAction::SetApiModelThinking { provider: "anthropic".into(), id: "claude-opus-5-5".into(), level: "high".into() }));
    let before = emitted.borrow().len();
    unsafe { let _: () = msg_send![&*controller, apiThinking: &*opus]; }
    check("the same level again sends nothing", emitted.borrow().len() == before);
    sonnet.setSelectedSegment(0);
    unsafe { let _: () = msg_send![&*controller, apiThinking: &*sonnet]; }
    check("Off on Sonnet tells the app", last() == Some(UiAction::SetApiModelThinking { provider: "anthropic".into(), id: "claude-sonnet-5-5".into(), level: "off".into() }));
    let engine = super::with_state_now(|s| { s.rebuild_menu(); s.engine_thinking.clone() }).flatten().unwrap();
    check("the local model's thinking: Off to High, at Low", titles(&engine) == ["Off", "Low", "Medium", "High"] && engine.selectedSegment() == 1);
    engine.setSelectedSegment(3);
    unsafe { let _: () = msg_send![&*controller, engineThinking: &*engine]; }
    check("High for the local model tells the app", last() == Some(UiAction::SetEngineThinking("high".into())));
    super::with_state_now(|s| { s.key_panel = None; s.apply(UiUpdate::ApiProviders(crate::demo_anthropic_providers(ProviderAuth::Missing))); s.rebuild_menu(); });
    let before = emitted.borrow().len();
    let connect = super::util::menu_item("Connect Anthropic…", None, None, "", None, mtm);
    connect.setTag(1);
    unsafe { let _: () = msg_send![&*controller, connectProvider: &*connect]; }
    let panel = super::with_state_now(|s| s.key_panel.as_ref().map(|p| (p.provider.clone(), p.mode, p.field.isHidden(), p.use_key.isHidden(), p.console.isHidden(), p.panel.clone()))).flatten();
    check("Connect Anthropic… opens the paste field, no sign-in", emitted.borrow().len() == before && panel.as_ref().is_some_and(|(p, m, field, use_key, console, _)| p == "anthropic" && *m == super::provider_key::Mode::Key && !field && *use_key && !console));
    if let Some((.., window)) = panel { window.close(); }
    // [menu lane] The built-in engine's row opens like a cloud model's: context from 128K to its window and thinking.
    super::with_state_now(|s| { s.engine.context_limit = Some(262_144); s.apply(UiUpdate::ApiProviders(crate::demo_all_providers(ProviderAuth::Connected))); s.rebuild_menu(); });
    let (disclosure, items, slider) = super::with_state_now(|s| (s.api_rows.engine_detail.0.clone(), s.api_rows.engine_detail.1.clone(), s.api_rows.engine_slider.clone())).unwrap();
    check("the local model's details start collapsed", disclosure.as_ref().is_some_and(|d| d.state() == NSControlStateValueOff) && !items.is_empty() && items.iter().all(|i| i.isHidden()));
    if let Some(d) = &disclosure { unsafe { d.performClick(None) }; }
    check("its disclosure shows its context and thinking", items.iter().all(|i| !i.isHidden()) && super::with_state_now(|s| s.engine_thinking.is_some()).unwrap());
    let Some((slider, field, steps)) = slider else { super::STATE.with(|s| *s.borrow_mut() = previous); drop(check); return failures };
    check("the local slider runs 128K to the engine's 262K window, at the whole window by default", steps == crate::model::context_steps(262_144) && steps.first() == Some(&128_000) && steps.last() == Some(&262_144)
        && slider.integerValue() as usize == steps.len() - 1 && field.stringValue().to_string() == "Context 262K");
    slider.setIntegerValue(0);
    unsafe { let _: () = msg_send![&*controller, engineContextSlider: &*slider]; }
    check("a local context stop goes to the app", last() == Some(UiAction::SetEngineContext(128_000)) && field.stringValue().to_string() == "Context 128K");
    let before = emitted.borrow().len();
    unsafe { let _: () = msg_send![&*controller, engineContextSlider: &*slider]; }
    check("the same local stop again sends nothing", emitted.borrow().len() == before);
    // Add Provider: closed while a cloud provider is connected; its header opens it in place, split private / online.
    super::with_state_now(|s| { s.apply(UiUpdate::ApiProviders(crate::demo_anthropic_providers(ProviderAuth::Connected))); s.rebuild_menu(); });
    let section = super::with_state_now(|s| s.add_provider_rows.get(&super::menu::Picker::Agent).cloned()).flatten();
    let Some((header, rows)) = section else { check("Agent ▸ Model has Add Provider", false); super::STATE.with(|s| *s.borrow_mut() = previous); drop(check); return failures };
    check("Add Provider starts closed with Anthropic connected", !header.is_on() && rows.iter().all(|i| i.isHidden()));
    super::check_row_clicked(super::menu::Picker::Agent.row());
    let titles: Vec<String> = rows.iter().map(|i| i.title().to_string()).collect();
    check(&format!("its header opens it in place: what is left, in Add Provider's order {titles:?}"), header.is_on() && rows.iter().all(|i| !i.isHidden())
        && titles == [super::provider_rows::CLAUDE_VIA_T3, "Codex subscription…"]);
    let chatgpt = rows.iter().find(|i| i.title().to_string() == "Codex subscription…");
    check("ChatGPT… connects like any provider, with ChatGPT's mark", chatgpt.is_some_and(|i| i.action() == Some(objc2::sel!(connectProvider:)) && i.image().is_some_and(|m| m.isTemplate())));
    super::check_row_clicked(super::menu::Picker::Agent.row());
    check("a second click closes it and the choice holds for the next open", !header.is_on() && rows.iter().all(|i| i.isHidden())
        && super::with_state_now(|s| { s.rebuild_menu(); s.add_provider_rows.get(&super::menu::Picker::Agent).is_some_and(|(h, _)| !h.is_on()) }).unwrap());
    // Background Tasks ▸ Model: Haiku on Anthropic is the default; the local model and the added cloud models are choices; a
    // provider without a key is refused.
    let islands = || super::with_state_now(|s| s.background_rows.models.iter().map(|(b, p, i)| (b.clone(), p.clone(), i.clone())).collect::<Vec<_>>()).unwrap();
    let rows = islands();
    let on: Vec<(String, String)> = rows.iter().filter(|(b, ..)| b.state() == NSControlStateValueOn).map(|(_, p, i)| (p.clone(), i.clone())).collect();
    check("Background Tasks ▸ Model: Claude Haiku 5.5 on Anthropic by default", on == [("anthropic".to_string(), "claude-haiku-5-5".to_string())]
        && super::with_state_now(|s| s.background_caption.as_ref().map(|c| c.text())).flatten().as_deref() == Some("Claude Haiku 5.5 · Anthropic API"));
    // [providers] No Add Provider in Background Tasks ▸ Model; its Add Model… opens Agent ▸ Model.
    // Background Tasks' own Model row (Agent and Speaking have one each above it).
    let islands_menu = super::with_state_now(|s| {
        let items = s.menu.itemArray();
        let at = items.iter().position(|i| i.title().to_string() == super::menu::BACKGROUND_TITLE)?;
        items.iter().skip(at).find(|i| super::menu::row_title(i) == super::menu::BACKGROUND_MODEL_TITLE).and_then(|i| i.submenu())
    }).flatten();
    let add = islands_menu.as_ref().and_then(|m| m.itemArray().iter().find(|i| i.title().to_string() == super::menu::BACKGROUND_ADD_MODEL));
    check("Background Tasks ▸ Model has no Add Provider, only Add Model…", islands_menu.as_ref().is_some_and(|m| m.itemArray().iter().all(|i| i.title().to_string() != super::menu::ADD_PROVIDER && i.title().to_string() != "Codex subscription…"))
        && add.as_ref().is_some_and(|i| i.action() == Some(objc2::sel!(openAgentModels:))) && !super::with_state_now(|s| s.add_provider_rows.contains_key(&super::menu::Picker::Background)).unwrap());
    if let Some(add) = &add { unsafe { let _: () = msg_send![&*controller, openAgentModels: &**add]; } }
    check("Add Model… asks for Agent ▸ Model", super::with_state_now(|s| std::mem::take(&mut s.agent_models_requested)).unwrap());
    // The menu stretches the caption to its width: read whole, no tooltip; squeezed, the whole text in one.
    let caption = super::with_state_now(|s| s.background_caption.clone()).flatten().unwrap();
    caption.setFrameSize(objc2_foundation::NSSize::new(super::controls::TILE_ROW_WIDTH, caption.frame().size.height));
    let whole = !caption.has_tooltip();
    caption.setFrameSize(objc2_foundation::NSSize::new(110.0, caption.frame().size.height));
    check("the Background Tasks caption has a tooltip only when cut", whole && caption.has_tooltip());
    let local = rows.iter().find(|(_, p, _)| p == crate::model::LOCAL_ENGINE).map(|(b, ..)| b.clone());
    check("the built-in engine is a background model choice", local.is_some());
    if let Some(local) = local { unsafe { local.performClick(None) }; }
    check("choosing it tells the app and the caption follows", matches!(last(), Some(UiAction::SetBackgroundModel { provider, .. }) if provider == crate::model::LOCAL_ENGINE)
        && super::with_state_now(|s| s.background_caption.as_ref().map(|c| c.text())).flatten().is_some_and(|t| t.ends_with("· Local")));
    super::with_state_now(|s| { s.apply(UiUpdate::ApiProviders(crate::demo_anthropic_providers(ProviderAuth::Missing))); s.rebuild_menu(); });
    let haiku = islands().into_iter().find(|(_, p, i)| p == "anthropic" && i == "claude-haiku-5-5").map(|(b, ..)| b);
    // [background picker] The provider's row says why (`background_picker`), its models are greyed.
    let said = super::with_state_now(|s| s.background_rows.shape.iter().any(|p| p.id == "anthropic" && p.reason.as_deref() == Some("Not connected"))).unwrap();
    check("without Anthropic's key its Haiku says so and cannot be chosen", said && haiku.as_ref().is_some_and(|b| !b.isEnabled()));
    let before = emitted.borrow().len();
    if let Some(haiku) = haiku { unsafe { let _: () = msg_send![&*controller, chooseBackgroundModel: &*haiku]; } }
    check("a refused Background Tasks ▸ Model row sends nothing", emitted.borrow().len() == before);
    // Background Tasks: the section replaces Islands, one toggle per job under its Model row, every
    // job on by default; a click turns one off in place and tells the app, a second turns it back on.
    let titles = super::with_state_now(|s| s.menu.itemArray().iter().map(|i| super::menu::row_title(&i)).collect::<Vec<_>>()).unwrap();
    let section_at = titles.iter().position(|t| t == super::menu::BACKGROUND_TITLE);
    check("the menu has Background Tasks and no Islands section", section_at.is_some()
        && super::with_state_now(|s| s.menu.itemArray().iter().all(|i| !(i.isSectionHeader() && i.title().to_string() == "Islands"))).unwrap());
    let expected: Vec<String> = [super::menu::BACKGROUND_TITLE, "", super::menu::BACKGROUND_MODEL_TITLE].into_iter().map(String::from)
        // Climb 2: one row per task (its switch and its own model inside).
        .chain(crate::model::BackgroundTask::ALL.iter().map(|t| t.title().to_string())).collect();
    check(&format!("Background Tasks: header, caption, Model, then one row per task ({:?})", section_at.map(|i| &titles[i..(i + expected.len()).min(titles.len())])),
        section_at.is_some_and(|i| titles.get(i..i + expected.len()).is_some_and(|got| got.iter().zip(&expected).all(|(g, e)| e.is_empty() || g == e))));
    let task_on = |t: crate::model::BackgroundTask| super::with_state_now(|s| s.background_task_rows.iter().find(|(k, _)| *k == t).map(|(_, r)| r.is_on())).flatten();
    check("every background task is on by default", crate::model::BackgroundTask::ALL.into_iter().all(|t| task_on(t) == Some(true)));
    super::check_row_clicked(super::menu::background_task_row(crate::model::BackgroundTask::VoiceNames));
    check("clicking Voice Names turns it off and tells the app", task_on(crate::model::BackgroundTask::VoiceNames) == Some(false)
        && task_on(crate::model::BackgroundTask::Islands) == Some(true)
        && last() == Some(UiAction::SetBackgroundTask { task: crate::model::BackgroundTask::VoiceNames, on: false }));
    let islands_row = super::menu::background_task_row(crate::model::BackgroundTask::Islands) as isize;
    // Climb 2: the switch's item heads the task's own submenu.
    let item = super::with_state_now(|s| s.task_pickers.iter().flat_map(|p| p.menu.itemArray().iter().collect::<Vec<_>>())
        .find(|i| i.tag() == islands_row && i.action() == Some(objc2::sel!(toggleBackgroundTask:)))).flatten();
    if let Some(item) = &item { unsafe { let _: () = msg_send![&*controller, toggleBackgroundTask: &**item]; } }
    check("Return on Islands turns the refresher off", last() == Some(UiAction::SetBackgroundTask { task: crate::model::BackgroundTask::Islands, on: false }));
    super::check_row_clicked(super::menu::background_task_row(crate::model::BackgroundTask::VoiceNames));
    super::check_row_clicked(super::menu::background_task_row(crate::model::BackgroundTask::Islands));
    check("a second click turns each back on", crate::model::BackgroundTask::ALL.into_iter().all(|t| task_on(t) == Some(true))
        && last() == Some(UiAction::SetBackgroundTask { task: crate::model::BackgroundTask::Islands, on: true }));
    // [providers] Speaking ▸ Voice ("when we click on one of those voices, the entire menu should
    // stay"): every voice is a radio row (view rows keep the menu tracking), a click sends its choice, the checks move
    // in place on the same rows, and a click on the voice already chosen sends it again (the app previews it).
    {
        use crate::speech_voice::{demo_speech_voice, LocalVoice, SpeechVoiceAction};
        super::with_state_now(|s| { s.speech_voice = demo_speech_voice(false, LocalVoice::Phonon, true); s.rebuild_menu(); });
        let buttons = || super::with_state_now(|s| super::speaking_menu::voice_buttons(&s.speaking_rows)).unwrap();
        let first = buttons();
        let names: Vec<&str> = first.iter().map(|(n, _)| n.as_str()).collect();
        // [voices] Phonon S is the one built-in voice; Own Engine's voices beside it.
        check(&format!("Speaking ▸ Voice: Phonon S, Own Engine and ElevenLabs, every voice a radio row ({} rows)", names.len()), !names.contains(&"local:Drama 3") && !names.contains(&"local:Supertonic") && names.contains(&"local:Phonon S")
            && names.contains(&"own") && names.contains(&"own:am_adam") && names.contains(&"api") && names.contains(&"preset:Edwin") && names.contains(&"preset:Bryony") && names.contains(&"voice:pNInz6obpgDQGcFmaJgB") && names.len() == 1 + 1 + 3 + 1 + 4 + 3);
        let on = |name: &str| buttons().into_iter().filter(|(_, b)| b.state() == NSControlStateValueOn).any(|(n, _)| n == name);
        let expect = |name: &str| -> SpeechVoiceAction {
            match name.split_once(':') {
                Some(("local", t)) => SpeechVoiceAction::SetLocal(LocalVoice::ALL.into_iter().find(|l| l.title() == t).unwrap()),
                Some(("preset", n)) => SpeechVoiceAction::SetPreset(n.into()),
                Some(("own", id)) => SpeechVoiceAction::SetOwnVoice { id: id.into(), name: demo_speech_voice(false, LocalVoice::Phonon, true).own_engine.voices.iter().find(|v| v.id == id).unwrap().name.clone() },
                _ if name == "own" => SpeechVoiceAction::SetOwnVoice { id: "af_heart".into(), name: "Heart".into() },
                Some(("voice", id)) => SpeechVoiceAction::SetElevenLabsVoice { id: id.into(), name: crate::speech_voice::demo_speech_voice(true, LocalVoice::Drama3, true).elevenlabs.voices.iter().find(|v| v.id == id).unwrap().name.clone() },
                _ => SpeechVoiceAction::SetElevenLabsVoice { id: "pNInz6obpgDQGcFmaJgB".into(), name: "Adam".into() },
            }
        };
        let mut every = true;
        for (name, button) in &first {
            if name == "api" || name == "own" { continue; }
            for again in [false, true] {
                let before = emitted.borrow().len();
                unsafe { button.performClick(None) };
                let sent = emitted.borrow().len() == before + 1 && last() == Some(UiAction::SpeechVoice(expect(name)));
                let same_rows = buttons().iter().zip(&first).all(|((_, a), (_, b))| Retained::as_ptr(a) == Retained::as_ptr(b));
                // A preset is Phonon S's: choosing one checks the preset and Phonon S.
                let checked = on(name) && (!name.starts_with("preset:") || on("local:Phonon S")) && (!name.starts_with("voice:") || on("api")) && (!name.starts_with("own:") || on("own"));
                if !(sent && same_rows && checked) { every = false; eprintln!("  voice row {name} (again={again}): sent={sent} same_rows={same_rows} checked={checked}"); }
            }
        }
        check("each voice row sends its choice, again on a re-click, and the checks move in place", every);
        // The ElevenLabs row (last chosen above: Adam) keeps the voice chosen.
        let api = first.iter().find(|(n, _)| n == "api").map(|(_, b)| b.clone()).unwrap();
        unsafe { api.performClick(None) };
        check("the ElevenLabs row sends its chosen voice", matches!(last(), Some(UiAction::SpeechVoice(SpeechVoiceAction::SetElevenLabsVoice { .. }))) && on("api"));
        let own = first.iter().find(|(n, _)| n == "own").map(|(_, b)| b.clone()).unwrap();
        unsafe { own.performClick(None) };
        check("the Own Engine row sends its chosen voice", matches!(last(), Some(UiAction::SpeechVoice(SpeechVoiceAction::SetOwnVoice { .. }))) && on("own") && !on("api"));
        // A saved Drama 3 keeps its row, checked, beside Phonon S; Supertonic is not offered.
        super::with_state_now(|s| { s.speech_voice = demo_speech_voice(false, LocalVoice::Drama3, true); s.rebuild_menu(); });
        let names: Vec<String> = buttons().into_iter().map(|(n, _)| n).collect();
        check("a saved Drama 3 keeps its row, checked, until another voice is chosen", on("local:Drama 3") && names.contains(&"local:Phonon S".to_string()) && !names.contains(&"local:Supertonic".to_string()));
        // Drama 3 is his private voice where it is installed, never in the public menu; on his Mac it
        // stays offered after he picks Phonon S, so he can go back.
        super::with_state_now(|s| { s.speech_voice = demo_speech_voice(false, LocalVoice::Phonon, true); s.speech_voice.drama3_installed = true; s.rebuild_menu(); });
        let names: Vec<String> = buttons().into_iter().map(|(n, _)| n).collect();
        check("installed Drama 3 stays offered after another voice is chosen", names.contains(&"local:Drama 3".to_string()) && on("local:Phonon S") && !on("local:Drama 3"));
        super::with_state_now(|s| { s.speech_voice = demo_speech_voice(false, LocalVoice::Phonon, true); s.speech_voice.drama3_installed = false; s.rebuild_menu(); });
        let names: Vec<String> = buttons().into_iter().map(|(n, _)| n).collect();
        check("the public build offers Phonon S as the one built-in voice", names.iter().filter(|n| n.starts_with("local:")).collect::<Vec<_>>() == ["local:Phonon S"]);
    }
    // Show Islands on Streams sits last in Seeing ▸ Recording, and every radio row there is as wide as its switch row,
    // so no row's right end falls outside its view (review, 24 Oct).
    let (recording, _, streams_row) = super::menu::recording_menu(&crate::ScreenRecording::default(), &[(3456, 2234)], false, None, mtm);
    let widths: Vec<f64> = recording.itemArray().iter().filter_map(|i| i.view()).map(|v| v.frame().size.width).collect();
    check("Show Islands on Streams is Recording's last row, every row as wide as it",
        recording.itemArray().lastObject().and_then(|i| i.view()).is_some_and(|v| std::ptr::eq(&*v as *const NSView as *const u8, &*streams_row as *const _ as *const u8))
            && widths.len() > 1 && widths.iter().all(|w| (w - streams_row.frame().size.width).abs() < 0.5));
    // Show Islands on Streams: off by default; a click shares the island's windows at once and tells the app;
    // the app's settings turn it off again.
    use objc2_app_kit::NSWindowSharingType;
    let streams_on = || super::with_state_now(|s| s.islands_on_streams_row.as_ref().map(|r| r.is_on())).flatten();
    check("Show Islands on Streams starts off, the island unshared", streams_on() == Some(false) && super::streams::sharing() == NSWindowSharingType::None);
    super::check_row_clicked(super::menu::ISLANDS_ON_STREAMS_ROW);
    check("a click turns it on: the island is shared and the app told", streams_on() == Some(true) && last() == Some(UiAction::SetIslandsOnStreams(true))
        && super::streams::sharing() == NSWindowSharingType::ReadOnly);
    super::with_state_now(|s| s.apply(UiUpdate::Settings(crate::Settings { islands_on_streams: false, ..s.settings_value.clone() })));
    check("the app's settings turn it off again", streams_on() == Some(false) && super::streams::sharing() == NSWindowSharingType::None);
    super::STATE.with(|s| *s.borrow_mut() = previous);
    failures
}

/// [F10, 27 Oct] The 26 Oct 22:07 and 22:08 flips (Luna, then Sol a second later; Terra, then Sol) came right after
/// the user added models. A click on Agent ▸ Model must select the model its row shows, once: while the menu tracks, a
/// snapshot that adds models (and reorders the list) leaves the open rows pointing at their own models; after the
/// next open, each new row selects itself; the row's press, its button and Return on its item each send one selection;
/// Background Tasks' Add Model… pop-up keeps its rows' clicks on their own models too.
fn api_switch_checks(mtm: MainThreadMarker) -> Vec<String> {
    use crate::{demo_anthropic_providers, demo_chatgpt_row, ApiModelRow, ProviderAuth, UiAction, UiUpdate};
    use objc2::msg_send;
    use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType, NSMenuItem};
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool| {
        eprintln!("check {name}: {}", if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_string()); }
    };
    let base = demo_anthropic_providers(ProviderAuth::Connected);
    let row = |provider: &str, id: &str, name: &str| {
        let mut r = base.models.iter().find(|m| m.provider == "anthropic" && m.id == "claude-haiku-5-5").unwrap().clone();
        (r.provider, r.id, r.name) = (provider.into(), id.into(), name.into());
        r
    };
    let (haiku, opus) = (row("anthropic", "claude-haiku-5-5", "Claude Haiku 5.5"), row("anthropic", "claude-opus-5-5", "Claude Opus 5.5"));
    let (sol, terra, luna) = (row("openai", "gpt-6.1-sol", "GPT-6.1 Sol"), row("openai", "gpt-6.1-terra", "GPT-6.1 Terra"), row("openai", "gpt-6-luna", "GPT-6 Luna"));
    let snapshot = |models: Vec<ApiModelRow>| {
        let mut api = base.clone();
        api.providers.retain(|p| p.id != "openai");
        api.providers.push(demo_chatgpt_row(ProviderAuth::Connected));
        api.models = models;
        api.selected = Some(("openai".into(), "gpt-6.1-sol".into()));
        api
    };
    let emitted = std::rc::Rc::new(std::cell::RefCell::new(Vec::<UiAction>::new()));
    let recorded = emitted.clone();
    let controller = super::Controller::new(mtm);
    let menu = objc2_app_kit::NSMenu::new(mtm);
    let mut state = super::State::new(mtm, controller.clone(), crate::Options { fake_permissions: true, ..Default::default() }, Box::new(move |a| recorded.borrow_mut().push(a)), None, menu, None);
    state.engine = engine_states().into_iter().find(|(n, _)| *n == "ready").unwrap().1;
    state.api = snapshot(vec![haiku.clone(), opus.clone(), sol.clone()]);
    state.rebuild_menu();
    let previous = super::STATE.with(|s| s.replace(Some(state)));
    // Every SelectApiModel since `from`, as (provider, id).
    let selections = |from: usize| emitted.borrow()[from..].iter().filter_map(|a| match a {
        UiAction::SelectApiModel { provider, id } => Some((provider.clone(), id.clone())),
        _ => None,
    }).collect::<Vec<_>>();
    let key = |m: &ApiModelRow| (m.provider.clone(), m.id.clone());
    // The live rows as (button, provider, id, the name the button shows).
    let rows = || super::with_state_now(|s| s.api_rows.models.iter().map(|(b, p, i)| (b.clone(), p.clone(), i.clone(), b.attributedTitle().string().to_string())).collect::<Vec<_>>()).unwrap();
    let shown = |m: &ApiModelRow| rows().into_iter().find(|(_, _, _, title)| title.contains(&m.name)).map(|(b, ..)| b);
    let reset = |m: &ApiModelRow| super::with_state_now(|s| s.api.selected = Some(key(m)));
    let names = rows().iter().map(|r| r.3.clone()).collect::<Vec<_>>();
    check(&format!("each row shows its own model: {names:?}"), rows().iter().all(|(_, p, i, title)| [&haiku, &opus, &sol].iter().any(|m| &m.provider == p && &m.id == i && title.contains(&m.name))));

    // The menu tracks; the user adds Terra and Luna and the app's snapshot lists ChatGPT's models first.
    super::with_state_now(|s| s.menu_open = true);
    let open_rows: Vec<usize> = rows().iter().map(|r| Retained::as_ptr(&r.0) as usize).collect();
    super::with_state_now(|s| s.apply(UiUpdate::ApiProviders(snapshot(vec![luna.clone(), terra.clone(), sol.clone(), haiku.clone(), opus.clone()]))));
    check("an open menu keeps its rows when models are added", rows().iter().map(|r| Retained::as_ptr(&r.0) as usize).collect::<Vec<_>>() == open_rows);
    for m in [&haiku, &opus] {
        let from = emitted.borrow().len();
        let Some(button) = shown(m) else { check(&format!("the open menu shows {}", m.name), false); continue };
        unsafe { button.performClick(None) };
        check(&format!("open menu, stale rows: {}'s row selects {} once ({:?})", m.name, m.id, selections(from)), selections(from) == vec![key(m)]);
        reset(&sol);
    }

    // The next open rebuilds (menuNeedsUpdate:): each new row selects its own model, once, by its press, its button
    // and Return on its item.
    super::with_state_now(|s| s.menu_open = false);
    let main = super::with_state_now(|s| s.menu.clone()).unwrap();
    unsafe { let _: () = msg_send![&*controller, menuNeedsUpdate: &*main]; }
    check("the next open lists Terra and Luna", shown(&terra).is_some() && shown(&luna).is_some());
    super::with_state_now(|s| s.menu_open = true);
    let model_menu = super::with_state_now(|s| s.api_item.as_ref().and_then(|i| i.submenu())).flatten().unwrap();
    let item_of = |button: &objc2_app_kit::NSButton| model_menu.itemArray().iter().find(|i| i.view().is_some_and(|v| unsafe { button.superview() }.is_some_and(|h| std::ptr::eq(&*v, &*h))));
    for m in [&luna, &terra, &haiku, &opus] {
        let Some(button) = shown(m) else { check(&format!("{} has a row", m.name), false); continue };
        // The row's press, as the menu delivers it (`ControlRow::mouseDown:`), then its release.
        let host = unsafe { button.superview() }.unwrap();
        let at = objc2_foundation::NSPoint::new(40.0, 14.0);
        for ty in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
            let from = emitted.borrow().len();
            let event = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(ty, at, NSEventModifierFlags(0), 0.0, 0, None, 0, 1, 1.0).unwrap();
            if ty == NSEventType::LeftMouseDown { unsafe { let _: () = msg_send![&*host, mouseDown: &*event]; } } else { unsafe { let _: () = msg_send![&*host, mouseUp: &*event]; } }
            let want = if ty == NSEventType::LeftMouseDown { vec![key(m)] } else { vec![] };
            check(&format!("{:?} on {}'s row: {:?}", ty, m.name, selections(from)), selections(from) == want);
        }
        reset(&sol);
        let from = emitted.borrow().len();
        unsafe { button.performClick(None) };
        check(&format!("{}'s button selects {} once ({:?})", m.name, m.id, selections(from)), selections(from) == vec![key(m)]);
        reset(&sol);
        let from = emitted.borrow().len();
        let item: Option<Retained<NSMenuItem>> = item_of(&button);
        let Some(item) = item else { check(&format!("{}'s row is an item of Agent ▸ Model", m.name), false); continue };
        unsafe { let _: () = msg_send![&*controller, chooseApiModel: &*item]; }
        check(&format!("Return on {}'s item selects {} once ({:?})", m.name, m.id, selections(from)), selections(from) == vec![key(m)]);
        reset(&sol);
    }
    // A second click on the chosen row sends nothing; a snapshot that still names Sol (the app's answer in flight)
    // sends nothing either.
    let luna_button = shown(&luna).unwrap();
    let from = emitted.borrow().len();
    unsafe { luna_button.performClick(None) };
    unsafe { luna_button.performClick(None) };
    super::with_state_now(|s| s.apply(UiUpdate::ApiProviders(snapshot(vec![sol.clone(), terra.clone(), luna.clone(), opus.clone(), haiku.clone()]))));
    check(&format!("a double click and a stale snapshot send one selection ({:?})", selections(from)), selections(from) == vec![key(&luna)]);

    // Background Tasks ▸ Model ▸ Add Model… pops up a fresh Agent ▸ Model (`popUpAgentModels:`) from a newer list.
    super::with_state_now(|s| { s.menu_open = false; s.api.selected = Some(key(&sol)); });
    let target = super::with_state_now(|s| s.target()).unwrap();
    let popup = super::with_state_now(|s| super::menu::agent_model_menu(s, Some(&target), mtm)).unwrap();
    let popup_names: Vec<String> = rows().iter().map(|r| r.3.clone()).collect();
    check(&format!("the pop-up lists the newer order: {popup_names:?}"), popup.numberOfItems() > 0 && rows().first().is_some_and(|r| r.2 == sol.id));
    for m in [&terra, &luna, &haiku] {
        let from = emitted.borrow().len();
        unsafe { shown(m).unwrap().performClick(None) };
        check(&format!("pop-up: {}'s row selects {} once ({:?})", m.name, m.id, selections(from)), selections(from) == vec![key(m)]);
        reset(&sol);
    }
    super::STATE.with(|s| *s.borrow_mut() = previous);
    failures
}
