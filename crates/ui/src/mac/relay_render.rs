//! `heimdall-ui-demo --claude-relay <dir>` (docs/claude-relay.md): the menu's Background Tasks in normal and
//! emergency mode (with and without a known reset), Agent ▸ Model with Claude Code subscription off and on, offscreen at 2×, and
//! the checks behind them: the pause shows and lifts in place while the menu is open, greyed rows take no click, the
//! relay row's check follows the setting and the model rows' checks clear. Nothing is ordered on screen.
use std::path::Path;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSControlStateValueOn, NSMenu, NSMenuItem};
use objc2_foundation::MainThreadMarker;

use super::offscreen::{demo_menu, stack};
use crate::model::{ProviderAuth, RelayIndicator, UiUpdate};

/// A reset at 21:15 local today, for the tooltip.
fn reset_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = now as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm.tm_hour = 21;
        tm.tm_min = 15;
        tm.tm_sec = 0;
        (libc::mktime(&mut tm) as u64) * 1000
    }
}

fn setup(s: &mut super::State, relay: RelayIndicator, agent_relay: bool) {
    let all = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
    s.apply(UiUpdate::Permissions(all));
    s.api = crate::demo_all_providers(ProviderAuth::Connected);
    // Background Tasks open (one big section open at a time).
    s.settings_value.collapsed = crate::model::MenuSection::ALL.iter().copied().filter(|x| *x != crate::model::MenuSection::Background).collect();
    s.engine.state = crate::EngineState::Off;
    s.settings_value.agent_relay = agent_relay;
    s.apply(UiUpdate::AgentRelay(relay));
}

/// The Background Tasks section of `menu` (its header to the separator after it) as a menu of its own.
fn section(menu: &NSMenu, mtm: MainThreadMarker) -> objc2::rc::Retained<NSMenu> {
    let out = NSMenu::new(mtm);
    out.setAutoenablesItems(false);
    let items = menu.itemArray();
    // Its header is a foldable section's row (30 Oct), titled as the section.
    let Some(start) = items.iter().position(|i| i.title().to_string() == super::menu::BACKGROUND_TITLE) else { return out };
    let moved: Vec<objc2::rc::Retained<NSMenuItem>> = items.iter().skip(start).take_while(|i| !i.isSeparatorItem()).collect();
    for item in moved {
        menu.removeItem(&item);
        out.addItem(&item);
    }
    out
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--claude-relay runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };
    let reset = Some(reset_ms());
    let states = [
        ("background-normal", RelayIndicator::Off),
        ("background-relay", RelayIndicator::Relay),
        ("background-emergency-reset", RelayIndicator::Emergency { reset_ms: reset }),
        ("background-emergency", RelayIndicator::Emergency { reset_ms: None }),
    ];
    for (name, relay) in states {
        for dark in [true, false] {
            let state = demo_menu(mtm, |s| setup(s, relay.clone(), false));
            let paused = state.background_paused.as_ref().unwrap();
            let emergency = relay.emergency();
            check(&format!("{name}: the greyed stand-in shows only in emergency mode"), paused.item.isHidden() != emergency, format!("{relay:?}"));
            check(&format!("{name}: the live toggles hide only in emergency mode"),
                state.background_task_rows.iter().all(|(_, r)| r.enclosingMenuItem().is_some_and(|i| i.isHidden() == emergency)), format!("{relay:?}"));
            // Climb 2: one native row per task (its switch and picker inside); in emergency mode the greyed stand-ins
            // take their place.
            check(&format!("{name}: the task rows give way to the greyed stand-ins only in emergency mode"),
                state.task_pickers.iter().all(|p| p.item.isHidden() == emergency), format!("{relay:?}"));
            // UX climb, 30 Oct: emergency reads on the headers too: Background Tasks says Paused in orange.
            let status = state.sections.get(&crate::model::MenuSection::Background).and_then(|(h, _)| h.status());
            let want = emergency.then(|| (super::menu::PAUSED.to_string(), super::controls::StatusTone::Warning));
            check(&format!("{name}: Background Tasks' header says Paused only in emergency mode"), status == want, format!("{status:?}"));
            let tip = paused.triangle.toolTip().map(|t| t.to_string());
            check(&format!("{name}: the triangle's tooltip"), tip == relay.paused_tooltip(), format!("{tip:?}"));
            if dark {
                if let Some(t) = &tip { println!("     tooltip: {t}") }
            }
            let suffix = if dark { "" } else { "-light" };
            let path = dir.join(format!("menu-{name}-section{suffix}.png"));
            // At the menu's real width (the tiles set it), as the user meets it.
            let view = super::offscreen::stacked(&section(&state.menu, mtm), super::offscreen::MENU_WIDTH, (6.0, 6.0), dark, mtm);
            let ok = super::offscreen::capture(&path, &view, dark);
            check(&path.display().to_string(), ok, "not written".into());
        }
    }
    // The whole menu once in emergency mode, at its real width, to see the section in place.
    let path = dir.join("menu-background-emergency-whole.png");
    let ok = super::offscreen::full_menu(&path, true, mtm, |s| setup(s, RelayIndicator::Emergency { reset_ms: reset }, false));
    check(&path.display().to_string(), ok, "not written".into());

    // In place while the menu is open: on, a reset learned, off.
    let mut state = demo_menu(mtm, |s| setup(s, RelayIndicator::Off, false));
    state.menu_open = true;
    state.apply(UiUpdate::AgentRelay(RelayIndicator::Emergency { reset_ms: None }));
    let hidden = |s: &super::State| s.background_paused.as_ref().unwrap().item.isHidden();
    check("open menu: emergency on greys the section in place", !hidden(&state), String::new());
    state.apply(UiUpdate::AgentRelay(RelayIndicator::Emergency { reset_ms: reset }));
    let tip = state.background_paused.as_ref().unwrap().triangle.toolTip().map(|t| t.to_string()).unwrap_or_default();
    check("open menu: a reset learned later names its time", tip.contains("21:15"), tip);
    let dimmed = state.background_paused.as_ref().unwrap().rows.iter().all(|(_, r)| r.is_dimmed());
    check("greyed rows are inert", dimmed, String::new());
    state.apply(UiUpdate::AgentRelay(RelayIndicator::Off));
    check("open menu: emergency off restores the live toggles", hidden(&state)
        && state.background_task_rows.iter().all(|(_, r)| r.enclosingMenuItem().is_some_and(|i| !i.isHidden())), String::new());

    // Agent ▸ Model with the Claude Code subscription (the relay) off and on.
    for (name, on) in [("agent-model", false), ("agent-model-relay", true)] {
        for dark in [true, false] {
            let mut state = demo_menu(mtm, |s| setup(s, RelayIndicator::Off, on));
            let target = state.target();
            let sub = super::menu::agent_model_menu(&mut state, Some(&*target), mtm);
            // Not added, the relay has no row in Cloud, only its Add Provider row; as the agent it has one.
            check(&format!("{name}: a Cloud row only once added"), state.relay_row.is_some() == on, String::new());
            let models_on = state.api_rows.models.iter().filter(|(b, _, _)| b.state() == NSControlStateValueOn).count()
                + state.api_rows.local.iter().filter(|b| b.state() == NSControlStateValueOn).count();
            check(&format!("{name}: one row checked"), models_on == usize::from(!on), format!("{models_on} model rows checked"));
            let add: Vec<String> = state.add_provider_rows.get(&super::menu::Picker::Agent).map(|(_, items)| items.iter().map(|i| i.title().to_string()).collect()).unwrap_or_default();
            let offered = add.iter().any(|t| t.starts_with(super::provider_rows::CLAUDE_VIA_T3));
            check(&format!("{name}: Add Provider offers the relay only while it is not added"), offered != on, format!("{add:?}"));
            let suffix = if dark { "" } else { "-light" };
            let path = dir.join(format!("menu-{name}{suffix}.png"));
            let ok = stack(&path, &sub, dark, mtm);
            check(&path.display().to_string(), ok, "not written".into());
            if let Some(relay) = state.relay_row.clone() {
                check(&format!("{name}: the relay row's check"), (relay.state() == NSControlStateValueOn) == on, String::new());
                // the relay is named as the subscription it is.
                let title = relay.attributedTitle().string().to_string();
                check(&format!("{name}: the relay row reads Claude Code subscription"), title.ends_with(super::menu::RELAY_TITLE) && super::menu::RELAY_TITLE == "Claude Code subscription", title);
            }
        }
    }
    failures
}
