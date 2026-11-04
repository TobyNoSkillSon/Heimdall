//! `heimdall-ui-demo --model-choice <dir>`: Agent ▸ Model's click-then-confirm and the provider fixes of 28 Oct
//! 17:26–17:33, driven through the real State and menu builder and rendered offscreen at 2× (nothing is ordered on
//! screen), with the checks behind them:
//! - a click on another row only proposes it: its details open, Confirm glows under them, nothing reaches the app;
//! - Confirm sends exactly what was clicked, back and forth (17:26: the relay after Luna loaded Luna);
//! - the relay (Claude Code subscription) opens to T3's Claude models; a model there is a choice like any other;
//! - closing the menu drops an unconfirmed click;
//! - Add Provider ▸ Local Splash Engine adds it to On This Mac and never switches the agent (17:26:11);
//! - (30 Oct 01:15) every provider says how it is reached, API or subscription, the same in Providers, Add Provider,
//!   the captions, the twin labels and Background Tasks ▸ Model; nothing added is offered again (the relay once on).
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSControlStateValueOn, NSMenu};
use objc2_foundation::MainThreadMarker;

use super::model_choice::{Choice, GLOW_PERIOD_S};
use super::offscreen::{demo_menu, stack};
use crate::model::{ApiModelDetail, ApiModelRow, ProviderAuth, RelayAccount, RelayModel, UiAction};

/// The user's list on 28 Oct 17:29 (his screenshot): Claude Opus, Sonnet and Haiku 5.5 by Anthropic key, GPT-6.1 Sol and
/// Terra and GPT-6 Luna on ChatGPT, Luna the agent's.
pub(crate) fn providers() -> crate::ApiProviders {
    let mut api = crate::demo_anthropic_providers(ProviderAuth::Connected);
    if let Some(p) = api.providers.iter_mut().find(|p| p.id == "openai") { p.auth = ProviderAuth::Connected; }
    api.models.retain(|m| m.provider == "anthropic");
    let gpt = |id: &str, name: &str| ApiModelRow {
        provider: "openai".into(), id: id.into(), name: name.into(), added: true, unavailable: None,
        detail: Some(ApiModelDetail { context_steps: vec![128_000, 192_000, 272_000], context: 272_000, price: None,
            thinking_levels: ["low", "medium", "high"].map(String::from).to_vec(), thinking: "medium".into() }),
    };
    api.models.extend([gpt("gpt-6.1-sol", "GPT-6.1 Sol"), gpt("gpt-6.1-terra", "GPT-6.1 Terra"), gpt("gpt-6-luna", "GPT-6 Luna")]);
    api.selected = Some(("openai".into(), "gpt-6-luna".into()));
    api
}

/// T3's Claude models as `orchestrator_capabilities` listed them on 28 Oct (the first four).
pub(crate) fn relay_models() -> Vec<RelayModel> {
    [("claude-opus-5-5", "Claude Opus 5.5"), ("claude-sonnet-5-5", "Claude Sonnet 5.5"), ("claude-fable-5-1", "Claude Fable 5.1"), ("claude-opus-5", "Claude Opus 5")]
        .map(|(id, name)| RelayModel { id: id.into(), name: name.into() }).to_vec()
}

type Sent = Rc<RefCell<Vec<UiAction>>>;

/// The demo menu in the user's state, open, with every action it sends kept in the returned list.
fn open_state(mtm: MainThreadMarker, engine_added: bool) -> (super::State, Sent) {
    let mut state = demo_menu(mtm, |s| {
        let all = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
        s.apply(crate::UiUpdate::Permissions(all));
        s.api = providers();
        s.engine.state = crate::EngineState::Off;
        s.relay_models = relay_models();
        s.settings_value.engine_added = engine_added;
        // The user's profile: the Claude Code subscription added as a provider (part 0 below checks it not added).
        s.settings_value.relay_added = true;
    });
    let sent: Sent = Default::default();
    let log = sent.clone();
    state.on_action = Box::new(move |a| log.borrow_mut().push(a));
    state.menu_open = true;
    (state, sent)
}

/// Agent ▸ Model as the open menu holds it now.
fn agent_menu(state: &super::State) -> objc2::rc::Retained<NSMenu> {
    state.api_item.as_ref().and_then(|i| i.submenu()).expect("Agent ▸ Model")
}

fn api_key(provider: &str, id: &str) -> (String, String) { (provider.into(), id.into()) }

fn checked(state: &super::State) -> Vec<String> {
    let mut on: Vec<String> = state.api_rows.models.iter().filter(|(b, _, _)| b.state() == NSControlStateValueOn).map(|(_, _, id)| id.clone()).collect();
    if state.api_rows.local.as_ref().is_some_and(|b| b.state() == NSControlStateValueOn) { on.push("engine".into()); }
    if state.relay_row.as_ref().is_some_and(|b| b.state() == NSControlStateValueOn) { on.push("relay".into()); }
    on
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--model-choice runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };
    // Each render draws a fresh Agent ▸ Model built from the state (the capture takes the rows' views out of their
    // menu), the Confirm pill's glow drawn at `glow` when there is one.
    let shot = |name: &str, state: &mut super::State, glow: f64, check: &mut dyn FnMut(&str, bool, String)| {
        let target = state.target();
        let menu = super::menu::agent_model_menu(state, Some(&*target), mtm);
        if let Some((_, view)) = &state.confirm_row { view.set_still(glow); }
        let path = dir.join(format!("{name}.png"));
        let ok = stack(&path, &menu, true, mtm);
        check(&format!("render {}", path.display()), ok, "not written".into());
        state.rebuild_menu();
    };

    // 1. At rest: Luna checked, Claude Code subscription closed over its models, no Confirm.
    let (mut state, sent) = open_state(mtm, false);
    check("rest: only Luna is checked", checked(&state) == ["gpt-6-luna"], format!("{:?}", checked(&state)));
    check("rest: no Confirm row", state.confirm_row.is_none(), String::new());
    check("rest: Claude Code subscription lists T3's models, folded", state.relay_model_rows.len() == 4 && state.relay_detail.1.iter().all(|i| i.isHidden()), format!("{}", state.relay_model_rows.len()));
    shot("agent-model-rest", &mut state, 0.0, &mut check);

    // 2. A click on Claude Opus 5.5: proposed, not sent; its details open with Confirm under them.
    state.choose(Choice::Api { provider: "anthropic".into(), id: "claude-opus-5-5".into() }, api_key("anthropic", "claude-opus-5-5"));
    check("click: nothing reaches the app", sent.borrow().is_empty(), format!("{:?}", sent.borrow()));
    check("click: the clicked row takes the check", checked(&state) == ["claude-opus-5-5"], format!("{:?}", checked(&state)));
    let opus = state.api_rows.models.iter().position(|(_, _, id)| id == "claude-opus-5-5").unwrap();
    check("click: its details open", state.api_rows.details[opus].1.iter().all(|i| !i.isHidden()), String::new());
    let menu = agent_menu(&state);
    let confirm_at = state.confirm_row.as_ref().map(|(i, _)| menu.indexOfItem(i));
    let last_detail = state.api_rows.details[opus].1.last().map(|i| menu.indexOfItem(i));
    check("click: Confirm sits right under the clicked row's details", confirm_at.is_some() && confirm_at == last_detail.map(|i| i + 1), format!("{confirm_at:?} after {last_detail:?}"));
    let glowing = state.confirm_row.as_ref().and_then(|(_, v)| v.layer()).and_then(|l| unsafe { l.animationForKey(&super::util::ns("confirm-glow")) });
    check("click: Confirm breathes (a 2.2 s glow, repeating)", glowing.as_ref().is_some_and(|a| {
        use objc2_quartz_core::CAMediaTiming;
        (a.duration() * 2.0 - GLOW_PERIOD_S).abs() < 1e-6 && a.autoreverses() && a.repeatCount().is_infinite()
    }), String::new());
    // The glow's breath at its low and peak, drawn into the pill for the still renders.
    shot("agent-model-pending-glow-low", &mut state, 0.0, &mut check);
    shot("agent-model-pending-glow-peak", &mut state, 1.0, &mut check);

    // 3. Confirm: Opus is sent, the check stays, Confirm leaves.
    state.confirm_choice();
    check("confirm: sends the clicked model", sent.borrow().as_slice() == [UiAction::SelectApiModel { provider: "anthropic".into(), id: "claude-opus-5-5".into() }], format!("{:?}", sent.borrow()));
    check("confirm: Confirm leaves", state.confirm_row.is_none(), String::new());
    sent.borrow_mut().clear();

    // 4. 17:26: back to Luna, then Claude Code subscription (its row): each Confirm sends what was clicked, never the one before.
    state.choose(Choice::Api { provider: "openai".into(), id: "gpt-6-luna".into() }, api_key("openai", "gpt-6-luna"));
    state.confirm_choice();
    let first = state.relay_models.first().map(|m| m.id.clone());
    state.choose(Choice::Relay(first.clone()), api_key(super::model_choice::RELAY_KEY.0, super::model_choice::RELAY_KEY.1));
    check("relay click: Claude Code subscription opens to its models with Confirm under them", state.relay_detail.1.iter().all(|i| !i.isHidden()) && state.confirm_row.is_some(), String::new());
    check("relay click: the relay and its first model are checked, Luna is not", checked(&state) == ["relay"]
        && state.relay_model_rows.iter().filter(|(b, _)| b.state() == NSControlStateValueOn).map(|(_, id)| id.as_str()).collect::<Vec<_>>() == ["claude-opus-5-5"], format!("{:?}", checked(&state)));
    shot("agent-model-relay-pending", &mut state, 1.0, &mut check);
    state.confirm_choice();
    check("back and forth: Luna, then the relay on Opus, each as clicked", sent.borrow().as_slice() == [
        UiAction::SelectApiModel { provider: "openai".into(), id: "gpt-6-luna".into() },
        UiAction::SelectRelayModel("claude-opus-5-5".into()),
    ], format!("{:?}", sent.borrow()));
    sent.borrow_mut().clear();
    // Another T3 model under the relay: a choice of its own.
    state.choose(Choice::Relay(Some("claude-sonnet-5-5".into())), api_key(super::model_choice::RELAY_KEY.0, super::model_choice::RELAY_KEY.1));
    state.confirm_choice();
    check("relay model: Sonnet through T3 is sent as the relay's model", sent.borrow().as_slice() == [UiAction::SelectRelayModel("claude-sonnet-5-5".into())], format!("{:?}", sent.borrow()));
    check("relay model: the relay stays checked on Sonnet", checked(&state) == ["relay"]
        && state.relay_model_rows.iter().filter(|(b, _)| b.state() == NSControlStateValueOn).map(|(_, id)| id.as_str()).collect::<Vec<_>>() == ["claude-sonnet-5-5"], String::new());
    shot("agent-model-relay-confirmed", &mut state, 0.0, &mut check);
    sent.borrow_mut().clear();

    // 5. A click left unconfirmed when the menu closes switches nothing; the next open shows what runs.
    state.choose(Choice::Api { provider: "openai".into(), id: "gpt-6.1-sol".into() }, api_key("openai", "gpt-6.1-sol"));
    state.pending_choice = None; // menuDidClose: (its with_state body)
    state.rebuild_menu();
    check("closed unconfirmed: nothing sent, the relay still checked", sent.borrow().is_empty() && checked(&state) == ["relay"], format!("{:?} {:?}", sent.borrow(), checked(&state)));

    // 6. Clicking what runs only opens or closes its details: nothing to confirm.
    state.choose(Choice::Relay(Some("claude-sonnet-5-5".into())), api_key(super::model_choice::RELAY_KEY.0, super::model_choice::RELAY_KEY.1));
    check("the running choice: no Confirm, nothing sent", state.confirm_row.is_none() && sent.borrow().is_empty(), String::new());

    // 7. Add Provider ▸ Local Splash Engine: On This Mac gains the engine; the agent stays on Luna.
    let (mut state, sent) = open_state(mtm, false);
    let listed = |s: &super::State| s.api_rows.local.is_some();
    check("engine not added: no On This Mac row", !listed(&state), String::new());
    shot("agent-model-engine-not-added", &mut state, 0.0, &mut check);
    state.settings_value.engine_added = true;
    state.emit(UiAction::AddBuiltInEngine);
    state.rebuild_agent_models();
    check("engine added: sends AddBuiltInEngine only (no engine start, no model switch)", sent.borrow().as_slice() == [UiAction::AddBuiltInEngine], format!("{:?}", sent.borrow()));
    check("engine added: listed under On This Mac, unchecked; Luna still the agent's", listed(&state) && checked(&state) == ["gpt-6-luna"], format!("{:?}", checked(&state)));
    let (mut state, _) = open_state(mtm, true);
    let add: Vec<String> = state.add_provider_rows.get(&super::menu::Picker::Agent).map(|(_, items)| items.iter().map(|i| i.title().to_string()).collect()).unwrap_or_default();
    check("engine added: it leaves Add Provider", !add.iter().any(|t| t.starts_with(super::menu::BUILT_IN_ENGINE)), format!("{add:?}"));
    shot("agent-model-engine-added", &mut state, 0.0, &mut check);
    // Its own submenu under Providers ends with Remove.
    let engine_sub = agent_menu(&state).itemArray().iter().find(|i| i.title().to_string() == super::menu::BUILT_IN_ENGINE && i.hasSubmenu()).and_then(|i| i.submenu());
    check("Providers ▸ Local Splash Engine ▸ Remove", engine_sub.is_some_and(|m| m.itemArray().iter().any(|i| i.title().to_string() == "Remove")), String::new());
    // 8. Nothing connected yet (no Providers header): Claude Code subscription still proposes, and Confirm shows in the live menu.
    let (mut state, sent) = open_state(mtm, false);
    state.api = crate::ApiProviders { unchosen: true, selected: None, models: vec![], catalog: vec![], ..crate::demo_all_providers(ProviderAuth::Missing) };
    for p in &mut state.api.providers { p.auth = ProviderAuth::Missing; }
    state.rebuild_menu();
    state.choose(Choice::Relay(Some("claude-opus-5-5".into())), api_key(super::model_choice::RELAY_KEY.0, super::model_choice::RELAY_KEY.1));
    let live = agent_menu(&state);
    check("nothing connected: Confirm shows in the open menu", state.confirm_row.as_ref().is_some_and(|(i, _)| live.indexOfItem(i) >= 0), String::new());
    state.confirm_choice();
    check("nothing connected: Confirm sends the relay", sent.borrow().as_slice() == [UiAction::SelectRelayModel("claude-opus-5-5".into())], format!("{:?}", sent.borrow()));
    shot("agent-model-blank-relay-confirmed", &mut state, 0.0, &mut check);
    // Blank, Add Provider open: the relay just confirmed is no longer offered; the other five say how they are reached.
    state.add_provider_choice.insert(super::menu::Picker::Agent, true);
    state.rebuild_menu();
    let add = add_titles(&state);
    check("blank, relay confirmed: Add Provider offers the other five, by their titles", add == ["Local Splash Engine", "Custom API", "OpenCode Go subscription\u{2026}", "Anthropic API\u{2026}", "Codex subscription\u{2026}"], format!("{add:?}"));
    shot("agent-model-blank-add-provider-open", &mut state, 0.0, &mut check);

    // 9. the access labels. Providers, captions and twins name how each is reached.
    let (mut state, sent) = open_state(mtm, true);
    state.settings_value.relay_added = false;
    state.add_provider_choice.insert(super::menu::Picker::Agent, true);
    state.rebuild_menu();
    let menu = agent_menu(&state);
    let providers: Vec<String> = menu.itemArray().iter().skip_while(|i| !(i.isSectionHeader() && i.title().to_string() == "Providers")).skip(1)
        .take_while(|i| !i.isSeparatorItem()).map(|i| i.title().to_string()).collect();
    check("Providers say API or subscription", providers == ["OpenCode Go subscription", "Codex subscription", super::menu::BUILT_IN_ENGINE, "Anthropic API"], format!("{providers:?}"));
    // Part 0 (it was in Cloud and Add Provider at once): not added, the relay is only in Add Provider.
    check("relay not added: no row in Cloud", state.relay_row.is_none(), String::new());
    let caption = super::menu::agent_caption(&super::model_choice::drawn(&state.api, &state.settings_value, None), "Qwen3.8 27B", "low");
    check("Agent caption: GPT-6 Luna · Medium · Codex subscription", caption == "GPT-6 Luna · Medium · Codex subscription", caption);
    let add = add_titles(&state);
    check("Add Provider: only what is not added (the relay)", add == [super::provider_rows::CLAUDE_VIA_T3], format!("{add:?}"));
    shot("agent-model-add-provider-open", &mut state, 0.0, &mut check);
    // Added (the row's press, as the app echoes it): the row joins Cloud, it leaves Add Provider, the agent stays on Luna.
    sent.borrow_mut().clear();
    // The row's action, and what its handler does (`claudeViaT3:`: the flag, then the action; this state is not the
    // controller's, so the render does it here).
    let row = state.add_provider_rows.get(&super::menu::Picker::Agent).and_then(|(_, items)| items.iter().find(|i| i.title().to_string() == super::provider_rows::CLAUDE_VIA_T3).cloned());
    check("Add Provider ▸ Claude Code subscription is an in-place row on claudeViaT3:", row.as_ref().is_some_and(|i| i.action() == Some(objc2::sel!(claudeViaT3:))
        && i.view().and_then(|v| v.downcast::<super::prominent::ProminentRow>().ok()).is_some_and(|r| r.is_in_place())), String::new());
    state.settings_value.relay_added = true;
    state.emit(UiAction::ClaudeViaT3);
    check("adding the relay sends ClaudeViaT3 only, never a model pick", sent.borrow().as_slice() == [UiAction::ClaudeViaT3], format!("{:?}", sent.borrow()));
    let added = crate::Settings { relay_added: true, ..state.settings_value.clone() };
    state.apply(crate::UiUpdate::Settings(added));
    state.rebuild_menu();
    let relay = state.relay_row.as_ref().map(|b| b.attributedTitle().string().to_string()).unwrap_or_default();
    check("relay added: its row in Cloud is the Claude Code subscription", relay.ends_with("Claude Code subscription"), relay);
    check("relay added: Add Provider no longer offers it; Luna is still the agent's", add_titles(&state).is_empty() && checked(&state) == ["gpt-6-luna"], format!("{:?} {:?}", add_titles(&state), checked(&state)));
    // Climb 2: Remove sits under the relay row's own models and accounts, or (no models listed yet) in its Providers row.
    let remove = |state: &super::State| agent_menu(state).itemArray().iter().find(|i| i.title().to_string() == "Remove" && i.action() == Some(objc2::sel!(removeClaudeViaT3:)))
        .or_else(|| agent_menu(state).itemArray().iter().find(|i| i.title().to_string() == super::menu::RELAY_TITLE && i.hasSubmenu()).and_then(|i| i.submenu())
            .and_then(|m| m.itemArray().iter().find(|i| i.title().to_string() == "Remove")));
    let twins = agent_menu(&state).itemArray().iter().filter(|i| i.view().is_none() && i.title().to_string() == super::menu::RELAY_TITLE).count();
    check("relay added: Remove is offered, and the relay is listed once", remove(&state).is_some_and(|i| i.isEnabled()) && (twins == 0 || state.relay_models.is_empty()), format!("{twins} twin rows"));
    check("all added: the menu ends on Anthropic API, no stray separator", agent_menu(&state).itemArray().lastObject().is_some_and(|i| i.title().to_string() == "Anthropic API"), String::new());
    shot("agent-model-relay-added", &mut state, 0.0, &mut check);
    // The relay on and then off: still added, so never offered again; Remove is greyed while it is the agent.
    let on = crate::Settings { agent_relay: true, ..state.settings_value.clone() };
    state.apply(crate::UiUpdate::Settings(on));
    state.rebuild_menu();
    check("relay on: Remove is greyed", remove(&state).is_some_and(|i| !i.isEnabled()), String::new());
    let off = crate::Settings { agent_relay: false, ..state.settings_value.clone() };
    state.apply(crate::UiUpdate::Settings(off));
    check("relay off, still added: Add Provider does not offer it", add_titles(&state).is_empty(), format!("{:?}", add_titles(&state)));

    // The captions above the Model rows are never cut: one too wide for the menu's narrowest width takes the provider
    // to a second line, and each line fits.
    let room = super::controls::TILE_ROW_WIDTH - 39.0 - 16.0;
    for line in ["GPT-6 Luna · Medium · Codex subscription", "Claude Haiku 5.5 · Low · OpenCode Go subscription", "Claude Opus 5.5 · High · Anthropic API",
        "Qwen3.8 27B · Low · Local Splash Engine", "Claude Haiku 5.5 · OpenCode Go subscription", "Claude Sonnet 5.5 · Thinking Off · OpenCode Go subscription"] {
        let shown = super::menu::caption_lines(line);
        let widths: Vec<f64> = shown.lines().map(|l| super::util::text_width(l, &super::util::menu_font())).collect();
        check(&format!("caption uncut: {shown:?} ({widths:.0?} of {room:.0} pt)"), widths.iter().all(|w| *w <= room) && shown.replace('\n', " \u{b7} ") == line, String::new());
    }
    // The open menu's Agent caption with the longest provider: two lines, no tooltip (nothing cut).
    let (mut state, _) = open_state(mtm, true);
    state.api.selected = Some(("opencode-go".into(), "claude-haiku-5-5".into()));
    state.api.models.push(crate::demo_api_providers(ProviderAuth::Connected, false).models.remove(0));
    state.rebuild_menu();
    let row = state.session_item.as_ref().map(|(_, r)| r.clone()).unwrap();
    check("Agent caption, OpenCode Go: two whole lines", row.text() == "Claude Haiku 5.5 \u{b7} Low\nOpenCode Go subscription" && !row.has_tooltip(), row.text());
    let path = dir.join("menu-agent-caption-two-lines.png");
    let api = state.api.clone();
    check(&format!("render {}", path.display()), super::offscreen::full_menu(&path, true, mtm, move |s| {
        s.api = api;
        s.settings_value.background_model = ("opencode-go".into(), "claude-haiku-5-5".into());
    }), "not written".into());
    // 10. Background Tasks ▸ Model: the same names. Haiku on two providers says which; the caption names the provider.
    let engine = super::offscreen::engine_states().into_iter().find(|(n, _)| *n == "ready").map(|(_, e)| e).unwrap_or_default();
    let mut api = crate::demo_anthropic_providers(ProviderAuth::Connected);
    if let Some(p) = api.providers.iter_mut().find(|p| p.id == "openai") { p.auth = ProviderAuth::Connected; }
    api.models.extend(providers_luna());
    let haiku = (crate::model::BACKGROUND_DEFAULT.0.to_string(), crate::model::BACKGROUND_DEFAULT.1.to_string());
    let caption = super::menu::background_caption(&api, &engine, &haiku);
    check("Background caption: Claude Haiku 5.5 · Anthropic API", caption == "Claude Haiku 5.5 · Anthropic API", caption);
    let local = super::menu::background_caption(&api, &engine, &(crate::model::LOCAL_ENGINE.to_string(), engine.model_id.clone()));
    check("Background caption, local: … · Local Splash Engine", local.ends_with(" · Local Splash Engine"), local);
    // Background Tasks ▸ Model's rows themselves: background_picker_render.
    failures
}

/// GPT-6 Luna on the Codex subscription, for Background Tasks ▸ Model's cloud rows.
fn providers_luna() -> Vec<ApiModelRow> {
    providers().models.into_iter().filter(|m| m.id == "gpt-6-luna").collect()
}

/// Agent ▸ Model ▸ Add Provider's row titles as the open menu holds them.
fn add_titles(state: &super::State) -> Vec<String> {
    state.add_provider_rows.get(&super::menu::Picker::Agent).map(|(_, items)| items.iter().filter(|i| !i.isSectionHeader()).map(|i| i.title().to_string()).collect()).unwrap_or_default()
}

/// `heimdall-ui-demo --relay-account <dir>`: Agent ▸ Model ▸ Claude Code subscription's accounts under its
/// models (Automatic, then each T3 Claude instance by T3's name), click then Confirm like the other rows, rendered
/// offscreen at 2×.
pub(crate) fn render_accounts(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--relay-account runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };
    let shot = |name: &str, state: &mut super::State, glow: f64, check: &mut dyn FnMut(&str, bool, String)| {
        let target = state.target();
        let menu = super::menu::agent_model_menu(state, Some(&*target), mtm);
        if let Some((_, view)) = &state.confirm_row { view.set_still(glow); }
        let path = dir.join(format!("{name}.png"));
        let ok = stack(&path, &menu, true, mtm);
        check(&format!("render {}", path.display()), ok, "not written".into());
        state.rebuild_menu();
    };
    let on_accounts = |s: &super::State| s.relay_account_rows.iter().filter(|(b, _)| b.state() == NSControlStateValueOn).map(|(_, id)| id.clone()).collect::<Vec<_>>();
    let on_models = |s: &super::State| s.relay_model_rows.iter().filter(|(b, _)| b.state() == NSControlStateValueOn).map(|(_, id)| id.clone()).collect::<Vec<_>>();
    let key = api_key(super::model_choice::RELAY_KEY.0, super::model_choice::RELAY_KEY.1);

    // The relay runs on Sonnet, Automatic; its disclosure open.
    let (mut state, sent) = open_state(mtm, false);
    state.relay_accounts = [("claudeAgent", "Claude"), ("claudeAgent_claude_code_2", "Claude Code 2")].map(|(id, name)| RelayAccount { id: id.into(), name: name.into() }).to_vec();
    state.settings_value.agent_relay = true;
    state.settings_value.agent_relay_model = Some("claude-sonnet-5-5".into());
    state.api_expanded.insert(key.clone());
    state.rebuild_menu();
    let titles: Vec<String> = state.relay_account_rows.iter().map(|(b, _)| b.title().to_string()).collect();
    check("rest: Automatic, then each T3 Claude instance by name", titles == ["Automatic", "Claude", "Claude Code 2"], format!("{titles:?}"));
    check("rest: Automatic and Sonnet checked", on_accounts(&state) == [None] && on_models(&state) == ["claude-sonnet-5-5"], format!("{:?} {:?}", on_accounts(&state), on_models(&state)));
    let menu = agent_menu(&state);
    let last_model = state.relay_model_rows.last().and_then(|(b, _)| b.enclosingMenuItem()).map(|i| menu.indexOfItem(&i));
    let first_account = state.relay_account_rows.first().and_then(|(b, _)| b.enclosingMenuItem()).map(|i| menu.indexOfItem(&i));
    check("rest: the accounts sit under the models, past one separator", last_model.zip(first_account).is_some_and(|(m, a)| a == m + 2), format!("{last_model:?} {first_account:?}"));
    shot("relay-account-rest", &mut state, 0.0, &mut check);

    // A click on the second account: proposed, not sent; Confirm under the relay's rows; Sonnet stays checked.
    state.choose(Choice::RelayAccount(Some("claudeAgent_claude_code_2".into())), key.clone());
    check("click: nothing reaches the app", sent.borrow().is_empty(), format!("{:?}", sent.borrow()));
    check("click: the account takes the check, the model stays", on_accounts(&state) == [Some("claudeAgent_claude_code_2".to_string())] && on_models(&state) == ["claude-sonnet-5-5"], format!("{:?}", on_accounts(&state)));
    let menu = agent_menu(&state);
    let confirm_at = state.confirm_row.as_ref().map(|(i, _)| menu.indexOfItem(i));
    let last = state.relay_detail.1.last().map(|i| menu.indexOfItem(i));
    check("click: Confirm right under the last account", confirm_at.is_some() && confirm_at == last.map(|i| i + 1), format!("{confirm_at:?} after {last:?}"));
    shot("relay-account-pending", &mut state, 1.0, &mut check);

    // Confirm: the account is sent alone; the relay runs on it.
    state.confirm_choice();
    check("confirm: sends the account", sent.borrow().as_slice() == [UiAction::SelectRelayAccount(Some("claudeAgent_claude_code_2".into()))], format!("{:?}", sent.borrow()));
    check("confirm: Confirm leaves, the account stays checked", state.confirm_row.is_none() && on_accounts(&state) == [Some("claudeAgent_claude_code_2".to_string())], String::new());
    shot("relay-account-confirmed", &mut state, 0.0, &mut check);
    sent.borrow_mut().clear();
    // Clicking the account it runs on: nothing to confirm. Automatic again: back to `None`.
    state.choose(Choice::RelayAccount(Some("claudeAgent_claude_code_2".into())), key.clone());
    check("the running account: no Confirm", state.confirm_row.is_none() && sent.borrow().is_empty(), String::new());
    state.choose(Choice::RelayAccount(None), key);
    state.confirm_choice();
    check("Automatic: sent as None", sent.borrow().as_slice() == [UiAction::SelectRelayAccount(None)], format!("{:?}", sent.borrow()));
    failures
}
