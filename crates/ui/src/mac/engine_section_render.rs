//! `heimdall-ui-demo --engine-section <dir>`: the Engine section, the Engine tile and the foldable sections (the user,
//! 30 Oct 01:15–01:23), driven through the real State and menu builder and rendered offscreen at 2× (nothing on screen),
//! with the checks behind them:
//! - the section header says every engine state in words (off, starting, loading with its spinner or ring, waiting for
//!   a pick in the soft accent, ready, failed with its reason whole on the line under it), and the tile shows the same
//!   state in one look of its own; a click on the tile always toggles (a failed engine turns off);
//! - a download in flight shows inline, greyed, with a spinner and a bar; off, the rows are grey; waiting, they rest on
//!   the soft accent;
//! - nothing in the section, the tile or Add Model ever sends an agent model change;
//! - Agent ▸ Model ▸ This Mac is one row, the engine, naming the model it holds;
//! - Agent, Engine, Personalisation and Background Tasks fold to their headers and open again in place, the menu open;
//!   the choice goes to the app and a rebuilt menu (the next launch) keeps it; an open section still hides what its own
//!   state hides (Agent off's caption).
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

use super::controls::StatusTone;
use super::engine_render::{section_menu, shoot_wide};
use super::offscreen::{demo_menu, full_menu, stack, tiles};
use crate::model::{EngineDownload, EngineState, EngineStatus, MenuSection, Tile, TileLook, UiAction, UiUpdate};

type Sent = Rc<RefCell<Vec<UiAction>>>;

/// The engine the user would meet: Qwen3.8 27B at UD-Q4_K_XL and Q8_0 on this Mac, Flint at Q4_K_M too, in `state`.
pub(crate) fn engine(state: EngineState) -> EngineStatus {
    let mut models = super::engine_render::splash_catalog();
    for m in &mut models {
        for q in &mut m.quants {
            q.downloaded = (m.id == "qwen3.8-27b" && (q.quant == "UD-Q4_K_XL" || q.quant == "Q8_0")) || (m.id == "flint" && q.quant == "Q4_K_M");
        }
    }
    let holds = state.holds_model() || matches!(state, EngineState::Error(_)) || state == EngineState::Off;
    EngineStatus {
        model_id: if holds { "qwen3.8-27b".into() } else { String::new() },
        model_label: if holds { "Qwen3.8 27B".into() } else { String::new() },
        context_limit: matches!(state, EngineState::Ready).then_some(262_144),
        state,
        models,
        port: 8100,
        memory_limit_bytes: Some(48 << 30),
        ..Default::default()
    }
}

fn open_state(mtm: MainThreadMarker, e: EngineStatus, setup: impl FnOnce(&mut super::State)) -> (super::State, Sent) {
    let mut state = demo_menu(mtm, |s| {
        let all = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
        s.apply(UiUpdate::Permissions(all));
        s.api = crate::ApiProviders { selected: None, ..crate::demo_anthropic_providers(crate::ProviderAuth::Connected) };
        s.engine = e;
        s.settings_value.engine_added = true;
        // The section under test open (one big section open at a time).
        s.settings_value.collapsed = MenuSection::ALL.iter().copied().filter(|x| *x != MenuSection::Engine).collect();
        setup(s);
    });
    let sent: Sent = Default::default();
    let log = sent.clone();
    state.on_action = Box::new(move |a| log.borrow_mut().push(a));
    state.menu_open = true;
    (state, sent)
}

/// Whether an action changes the agent's model.
fn picks_agent(a: &UiAction) -> bool {
    matches!(a, UiAction::SelectApiModel { .. } | UiAction::SelectEngineModel(_) | UiAction::SelectLocalAgent | UiAction::SelectAgentRelay
        | UiAction::SelectRelayModel(_) | UiAction::DownloadEngineModel { then_select: true, .. })
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--engine-section runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    super::controls::STILL_SPINNERS.with(|c| c.set(true));
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };
    let shoot = |name: &str, menu: &objc2_app_kit::NSMenu, check: &mut dyn FnMut(&str, bool, String)| {
        let path = dir.join(format!("{name}.png"));
        let ok = stack(&path, menu, true, mtm);
        check(&format!("render {}", path.display()), ok, "not written".into());
    };

    // 1. Every state: the header's words and tone, the reason line, the rows' look, and the tile beside it.
    let states = [
        ("off", EngineState::Off, "Off", StatusTone::Grey, None, TileLook::Off),
        ("starting", EngineState::Starting, "Starting", StatusTone::Grey, Some(None), TileLook::Loading(None)),
        ("waiting", EngineState::Waiting, "Pick a model", StatusTone::Accent, None, TileLook::Waiting),
        ("loading", EngineState::Loading { percent: None }, "Loading", StatusTone::Grey, Some(None), TileLook::Loading(None)),
        ("loading-42", EngineState::Loading { percent: Some(42) }, "Loading 42%", StatusTone::Grey, Some(Some(0.42)), TileLook::Loading(Some(42))),
        ("ready", EngineState::Ready, "Ready", StatusTone::Grey, None, TileLook::On),
        ("failed", EngineState::Error("Engine port is already occupied".into()), "Failed", StatusTone::Alert, None, TileLook::Problem),
    ];
    for (name, st, words, tone, progress, look) in states {
        let (state, sent) = open_state(mtm, engine(st.clone()), |_| {});
        let header = state.engine_rows.header.clone().unwrap();
        let spinner = header.status_spinner();
        let spin_ok = match progress {
            None => spinner.is_none(),
            Some(None) => spinner.as_ref().is_some_and(|s| s.isIndeterminate()),
            Some(Some(f)) => spinner.as_ref().is_some_and(|s| !s.isIndeterminate() && (s.doubleValue() - f).abs() < 1e-6),
        };
        check(&format!("{name}: the header says \"{words}\""), header.status() == Some((words.to_string(), tone)) && spin_ok, format!("{:?} spinner {:?}", header.status(), spinner.map(|s| s.isIndeterminate())));
        let tile = &state.tile_views[4];
        check(&format!("{name}: the tile shows it in one look, and spins only while on its way"), tile.look() == look && tile.spinner().is_some() == matches!(look, TileLook::Loading(_)), format!("{:?}", tile.look()));
        let reason = state.engine_rows.reason.as_ref().map(|(i, r)| (i.isHidden(), r.text()));
        let reason_ok = match &st { EngineState::Error(why) => reason == Some((false, why.clone())), _ => reason.as_ref().is_some_and(|(h, _)| *h) };
        check(&format!("{name}: a failure's reason shows whole under the header, nothing otherwise"), reason_ok, format!("{reason:?}"));
        let lit: Vec<bool> = state.engine_rows.picks.iter().map(|p| p.row.is_resting()).collect();
        let lit_ok = match st { EngineState::Waiting => lit.iter().all(|l| *l), _ => lit.iter().filter(|l| **l).count() == 1 };
        check(&format!("{name}: the loaded one rests lit (waiting: every row, the soft accent)"), lit_ok, format!("{lit:?}"));
        check(&format!("{name}: nothing is sent by building it"), sent.borrow().is_empty(), format!("{:?}", sent.borrow()));
        let path = dir.join(format!("section-{name}.png"));
        check(&format!("render {}", path.display()), shoot_wide(&path, &section_menu(&state, mtm), mtm), String::new());
        let path = dir.join(format!("tile-{name}.png"));
        let ok = tiles(&path, true, [true, true, true], look, false, true, mtm);
        check(&format!("render {}", path.display()), ok, String::new());
    }

    // 2. A download in flight, inline: greyed, a spinner and a bar; it joins the list when it lands.
    let mut e = engine(EngineState::Ready);
    e.models[1].download = Some(EngineDownload { quant: "Q4_K".into(), done_bytes: 9_100_000_000, total_bytes: Some(21_712_409_824), error: None });
    let (mut state, _) = open_state(mtm, e, |_| {});
    let row = state.engine_rows.picks.iter().find(|p| p.entry.id == "qwen3.6-35b-a3b").map(|p| (p.item.isEnabled(), p.bar.as_ref().map(|b| b.doubleValue()), p.row.subviews().len()));
    check("downloading: inline, greyed, its bar at 42% beside a spinner", row.is_some_and(|(enabled, bar, views)| !enabled && bar.is_some_and(|b| (b - 0.419).abs() < 0.01) && views >= 2), format!("{row:?}"));
    let path = dir.join("section-downloading.png");
    check(&format!("render {}", path.display()), shoot_wide(&path, &section_menu(&state, mtm), mtm), String::new());
    state.rebuild_menu();
    let before = state.engine_rows.picks.len();
    if let Some(m) = state.engine.models.iter_mut().find(|m| m.id == "qwen3.6-35b-a3b") {
        m.download = None;
        if let Some(q) = m.quants.iter_mut().find(|q| q.quant == "Q4_K") { q.downloaded = true; }
    }
    super::engine_section::relay(&mut state);
    super::menu::sync_tracking(&mut state);
    let landed = state.engine_rows.picks.iter().find(|p| p.entry.id == "qwen3.6-35b-a3b").map(|p| p.item.isEnabled());
    check("landed: it joins the quick-pick list in place, a pick like the others", landed == Some(true) && state.engine_rows.picks.len() == before
        && state.menu.indexOfItem(&state.engine_rows.picks[0].item) >= 0, format!("{landed:?}"));

    // 3. Clicks: a quick-pick loads, the tile toggles; neither touches the agent's model.
    let (mut state, sent) = open_state(mtm, engine(EngineState::Waiting), |_| {});
    let flint = state.engine_rows.picks.iter().position(|p| p.entry.id == "flint").unwrap();
    state.load_pick(flint);
    check("waiting, a pick loads that model and nothing else", sent.borrow().as_slice() == [UiAction::LoadEngineModel { id: "flint".into(), quant: "Q4_K_M".into() }], format!("{:?}", sent.borrow()));
    check("the header turns to Starting at once", state.engine_rows.header.as_ref().and_then(|h| h.status()).is_some_and(|(t, _)| t == "Starting"), String::new());
    sent.borrow_mut().clear();
    for (from, want) in [(EngineState::Error("x".into()), UiAction::SetEngine(false)), (EngineState::Off, UiAction::SetEngine(true)), (EngineState::Waiting, UiAction::SetEngine(false)),
        (EngineState::Loading { percent: None }, UiAction::SetEngine(false)), (EngineState::Ready, UiAction::SetEngine(false))] {
        state.engine.state = from.clone();
        sent.borrow_mut().clear();
        state.toggle_tile(Tile::Engine);
        check(&format!("tile click on {from:?}: {want:?}"), sent.borrow().as_slice() == [want.clone()], format!("{:?}", sent.borrow()));
    }
    let (mut state, sent) = open_state(mtm, engine(EngineState::Ready), |_| {});
    let i = state.engine_rows.precisions.iter().position(|p| p.id == "flint" && p.quant == "Q8_0").unwrap();
    state.precision(i, super::Press::Download);
    state.load_pick(0);
    check("nothing in the Engine section changes the agent's model", !sent.borrow().iter().any(picks_agent) && !sent.borrow().is_empty(), format!("{:?}", sent.borrow()));

    // 4. Agent ▸ Model ▸ This Mac: one row, the engine, naming the model it holds; no download or precision rows.
    let (mut state, _) = open_state(mtm, engine(EngineState::Ready), |_| {});
    let target = state.target();
    let agent = super::menu::agent_model_menu(&mut state, Some(&*target), mtm);
    let local = state.api_rows.local.as_ref().map(|b| b.attributedTitle().string().to_string());
    check("This Mac: the engine with the loaded model's name", local.as_deref().is_some_and(|t| t.contains("Local Splash Engine") && t.contains("Qwen3.8 27B")), format!("{local:?}"));
    shoot("agent-model-this-mac", &agent, &mut check);
    let (mut state, _) = open_state(mtm, engine(EngineState::Waiting), |_| {});
    let agent = super::menu::agent_model_menu(&mut state, Some(&*target), mtm);
    let local = state.api_rows.local.as_ref().map(|b| b.attributedTitle().string().to_string());
    check("This Mac, nothing loaded: the engine alone", local.as_deref() == Some("\u{fffc} Local Splash Engine") || local.as_deref() == Some("Local Splash Engine"), format!("{local:?}"));
    shoot("agent-model-this-mac-no-model", &agent, &mut check);

    // 5. Sections: one big section open at a time (UX climb, 30 Oct); each opens in place, folding the one that was
    // open, folds to its header, and the choice is sent and kept by a rebuild.
    let path = dir.join("menu-expanded.png");
    let ok = full_menu(&path, true, mtm, |s| { s.engine = engine(EngineState::Ready); s.settings_value.engine_added = true; });
    check(&format!("render {}", path.display()), ok, String::new());
    let (mut state, sent) = open_state(mtm, engine(EngineState::Ready), |s| s.settings_value.collapsed.clear());
    let open = |state: &super::State| MenuSection::ALL.iter().copied().filter(|s| state.sections.get(s).is_some_and(|(h, _)| h.is_on())).collect::<Vec<_>>();
    let titled = MenuSection::ALL.iter().all(|s| state.sections.get(s).and_then(|(h, _)| h.enclosingMenuItem()).is_some_and(|i| i.title().to_string() == s.title()));
    check("every section saved open: only the first shows open, each header a row of its own", titled && open(&state) == [MenuSection::Agent], format!("{:?}", open(&state)));
    for section in MenuSection::ALL {
        let row = super::menu::section_row(section);
        check(&format!("{} has its own symbol", section.title()), super::util::symbol(section.icon()).is_some() && super::menu::section_from_row(row) == Some(section), section.icon().into());
        if section != MenuSection::Agent {
            state.toggle_section(section);
            let shown = state.sections.get(&section).is_some_and(|(h, items)| h.is_on() && items.iter().any(|i| !i.isHidden()));
            check(&format!("{} opens in place and is the only one open", section.title()), shown && open(&state) == [section] && state.menu_open, format!("{:?}", open(&state)));
        }
        state.toggle_section(section);
        let (header, items) = state.sections.get(&section).cloned().unwrap();
        check(&format!("{} folds: its rows hide, its chevron points right, the menu stays open", section.title()),
            !header.is_on() && !items.is_empty() && items.iter().all(|i| i.isHidden()) && state.menu_open,
            format!("{} rows", items.len()));
    }
    // Every click sends one action, the whole folded list (the app's saved list then matches the menu, one echo).
    let lists: Vec<_> = sent.borrow().iter().filter_map(|a| match a { UiAction::SetSectionsCollapsed(l) => Some(l.clone()), _ => None }).collect();
    check("each click is sent to be kept, the whole folded list in one action", lists.len() == 2 * MenuSection::ALL.len() - 1 && open(&state).is_empty()
        && lists.last() == Some(&MenuSection::ALL.to_vec()), format!("{:?}", sent.borrow()));
    let path = dir.join("menu-collapsed.png");
    let container = super::offscreen::stacked(&state.menu, super::offscreen::MENU_WIDTH, (5.0, 0.5), true, mtm);
    check(&format!("render {}", path.display()), super::offscreen::capture_rounded(&path, &container, true), String::new());
    // The next launch: a menu built from the saved choice comes up folded.
    let (state2, _) = open_state(mtm, engine(EngineState::Ready), |s| s.settings_value.collapsed = MenuSection::ALL.to_vec());
    let folded = MenuSection::ALL.iter().all(|s| state2.sections.get(s).is_some_and(|(h, items)| !h.is_on() && items.iter().all(|i| i.isHidden())));
    check("a rebuilt menu (the next launch) keeps every section folded", folded, String::new());
    // Open again: the rows come back, except what the section's own state hides (Agent off's caption).
    let (mut state, sent) = open_state(mtm, engine(EngineState::Ready), |s| {
        s.settings_value.collapsed = MenuSection::ALL.to_vec();
        s.runtime_settings.dry_run = true;
    });
    state.toggle_section(MenuSection::Agent);
    let session_hidden = state.session_item.as_ref().is_some_and(|(i, _)| i.isHidden());
    let agent_rows = state.sections.get(&MenuSection::Agent).map(|(_, items)| items.iter().filter(|i| !i.isHidden()).count());
    check("Agent opens again: its rows show, Agent off's caption stays hidden", session_hidden && agent_rows == Some(2), format!("{agent_rows:?} caption hidden {session_hidden}"));
    state.toggle_section(MenuSection::Engine);
    let reason_hidden = state.engine_rows.reason.as_ref().is_some_and(|(i, _)| i.isHidden());
    let engine_rows = state.sections.get(&MenuSection::Engine).map(|(_, items)| items.iter().filter(|i| !i.isHidden()).count());
    let agent_rows = state.sections.get(&MenuSection::Agent).map(|(_, items)| items.iter().filter(|i| !i.isHidden()).count());
    check("Engine opens again: its picks and Add Model show, no reason line while it runs; Agent folds", reason_hidden && engine_rows == Some(4) && agent_rows == Some(0), format!("{engine_rows:?} {agent_rows:?}"));
    let lists: Vec<_> = sent.borrow().iter().filter_map(|a| match a { UiAction::SetSectionsCollapsed(l) => Some(l.clone()), _ => None }).collect();
    check("each opening is sent to be kept, with the fold it caused", lists == [super::menu::folded_after_click(MenuSection::Agent, false), super::menu::folded_after_click(MenuSection::Engine, false)], format!("{:?}", sent.borrow()));
    let path = dir.join("menu-engine-open-others-folded.png");
    let ok = full_menu(&path, true, mtm, |s| {
        s.engine = engine(EngineState::Loading { percent: None });
        s.settings_value.engine_added = true;
        s.settings_value.collapsed = vec![MenuSection::Agent, MenuSection::Decider, MenuSection::Personalisation, MenuSection::Background];
    });
    check(&format!("render {}", path.display()), ok, String::new());
    super::controls::STILL_SPINNERS.with(|c| c.set(false));
    failures
}
