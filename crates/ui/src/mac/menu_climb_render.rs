//! `heimdall-ui-demo --menu-climb <dir>` (lab/climb/menu-ux): the whole main menu in every state the UX climb
//! judges, offscreen at real size (2×), built by the real `menu::build` on the user's setup: every section open and
//! folded, a hovered header and row, long model names, a task on its fallback, emergency mode, the relay on and off,
//! and the light look. Nothing is ordered on screen; `menu-climb.txt` lists each render's height in points.
use std::path::Path;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::MainThreadMarker;

use super::offscreen::{capture_rounded, demo_menu, stacked, HOVER, MENU_WIDTH};
use crate::model::{ApiModelDetail, ApiModelRow, BackgroundTask, EngineState, MenuSection, RelayIndicator, UiUpdate};

/// The user's menu on 30 Oct: Anthropic's API and ChatGPT connected, the agent on GPT-6.1 Sol, the engine ready with
/// Qwen3.8 27B, the Claude Code subscription added, T3's Claude code 2 and Codex, GPT-6 Luna for background tasks
/// with the islands on Claude Haiku 5.5.
pub(crate) fn owner(s: &mut super::State) {
    super::background_picker_render::luna_setup(s);
    let luna = s.api.models.iter().find(|m| m.id == "gpt-6-luna").cloned();
    s.api.models = super::model_choice_render::providers().models;
    if let Some(l) = luna { if !s.api.models.iter().any(|m| m.id == l.id) { s.api.models.push(l) } }
    s.api.selected = Some(("openai".into(), "gpt-6.1-sol".into()));
    s.engine = super::engine_section_render::engine(EngineState::Ready);
    s.settings_value.engine_added = true;
    s.relay_models = super::model_choice_render::relay_models();
    s.settings_value.relay_added = true;
    // Every section saved open, as the user's profile was before the climb (luna_setup opens Background Tasks alone).
    s.settings_value.collapsed.clear();
}

/// Only `open` open (with every section open, as before the climb, the states below show it among the others).
fn only(s: &mut super::State, open: MenuSection) {
    s.settings_value.collapsed = MenuSection::ALL.iter().copied().filter(|x| *x != open).collect();
}

/// One state: its file name and what it changes on the user's menu.
type Setup = Box<dyn Fn(&mut super::State)>;

fn states() -> Vec<(&'static str, Setup)> {
    let fold = |open: &'static [MenuSection]| move |s: &mut super::State| {
        owner(s);
        s.settings_value.collapsed = MenuSection::ALL.iter().copied().filter(|x| !open.contains(x)).collect();
    };
    vec![
        ("01-all-open", Box::new(owner)),
        ("02-all-folded", Box::new(fold(&[]))),
        ("03-agent-open", Box::new(fold(&[MenuSection::Agent]))),
        ("04-background-open", Box::new(fold(&[MenuSection::Background]))),
        ("05-long-names", Box::new(|s: &mut super::State| {
            owner(s);
            let long = ApiModelRow { provider: "openai".into(), id: "gpt-6.1-sol-deep-research-preview".into(), name: "GPT-6.1 Sol Deep Research Preview".into(), added: true, unavailable: None,
                detail: Some(ApiModelDetail { context_steps: vec![272_000], context: 272_000, price: None, thinking_levels: ["low", "medium", "high"].map(String::from).to_vec(), thinking: "high".into() }) };
            s.api.models.push(long.clone());
            s.api.selected = Some((long.provider.clone(), long.id.clone()));
            s.settings_value.background_model = ("openai".into(), long.id.clone());
            s.engine.model_label = "Qwen3.8 Coder 480B A35B Instruct".into();
        })),
        ("05b-long-names-background", Box::new(|s: &mut super::State| {
            owner(s);
            s.api.models.push(ApiModelRow { provider: "openai".into(), id: "gpt-6.1-sol-deep-research-preview".into(), name: "GPT-6.1 Sol Deep Research Preview".into(), added: true, unavailable: None, detail: None });
            s.settings_value.background_model = ("openai".into(), "gpt-6.1-sol-deep-research-preview".into());
            only(s, MenuSection::Background);
        })),
        ("06-fallback", Box::new(|s: &mut super::State| {
            owner(s);
            only(s, MenuSection::Background);
            s.background_routes = vec![(BackgroundTask::Islands, "t3:claudeAgent".into(), "claude-haiku-5-5".into()), (BackgroundTask::Plan, "t3:claudeAgent_claude_code_2".into(), "claude-haiku-5-5".into())];
        })),
        ("07-emergency", Box::new(|s: &mut super::State| {
            owner(s);
            only(s, MenuSection::Background);
            s.apply(UiUpdate::AgentRelay(RelayIndicator::Emergency { reset_ms: None }));
        })),
        ("08-relay-on", Box::new(|s: &mut super::State| {
            owner(s);
            s.settings_value.agent_relay = true;
            s.settings_value.agent_relay_model = Some("claude-opus-5-5".into());
            s.apply(UiUpdate::AgentRelay(RelayIndicator::Relay));
        })),
        ("09-relay-off", Box::new(|s: &mut super::State| {
            owner(s);
            s.apply(UiUpdate::AgentRelay(RelayIndicator::Off));
        })),
        ("10-engine-loading", Box::new(|s: &mut super::State| {
            owner(s);
            only(s, MenuSection::Engine);
            s.engine = super::engine_section_render::engine(EngineState::Loading { percent: Some(42) });
        })),
        ("14-decider-open", Box::new(|s: &mut super::State| { owner(s); only(s, MenuSection::Decider); })),
        ("15-personalisation-open", Box::new(|s: &mut super::State| { owner(s); only(s, MenuSection::Personalisation); })),
        ("17-task-off", Box::new(|s: &mut super::State| {
            owner(s);
            only(s, MenuSection::Background);
            s.settings_value.background_off = vec![BackgroundTask::VoiceNames];
        })),
        ("16-fallback-folded", Box::new(|s: &mut super::State| {
            owner(s);
            s.settings_value.collapsed = MenuSection::ALL.to_vec();
            s.background_routes = vec![(BackgroundTask::Islands, "t3:claudeAgent".into(), "claude-haiku-5-5".into()), (BackgroundTask::Plan, "t3:claudeAgent_claude_code_2".into(), "claude-haiku-5-5".into())];
        })),
    ]
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--menu-climb runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    super::controls::STILL_SPINNERS.with(|c| c.set(true));
    let mut failures = vec![];
    let mut heights = String::new();
    let mut shot = |name: &str, dark: bool, setup: &dyn Fn(&mut super::State), hover: Option<&str>| {
        let state = demo_menu(mtm, setup);
        // AppKit's own layout width for the menu as built (`NSMenu.size`, no window): the width the user sees, the tiles'
        // in every state ("the width must never change").
        let appkit = state.menu.size().width;
        heights.push_str(&format!("{name} appkit-width {appkit:.1}\n"));
        if (appkit - super::controls::TILE_ROW_WIDTH).abs() > 0.5 {
            eprintln!("FAIL {name}: the menu is {appkit} pt wide, not {}", super::controls::TILE_ROW_WIDTH);
            failures.push(format!("{name} width {appkit}"));
        }
        match hover {
            // A section header (a `CheckRow`) by its title, else a native row by its title.
            Some(title) => {
                let header = state.sections.values().find(|(h, _)| h.enclosingMenuItem().is_some_and(|i| i.title().to_string() == title));
                match header { Some((h, _)) => h.hover_for_render(), None => HOVER.with(|c| *c.borrow_mut() = Some(title.into())) }
            }
            None => HOVER.with(|c| *c.borrow_mut() = None),
        }
        // The width AppKit lays the menu out at (`NSMenu.size`), as the user sees it.
        let width = state.menu.size().width.max(MENU_WIDTH).ceil();
        let container = stacked(&state.menu, width, (5.0, 0.5), dark, mtm);
        HOVER.with(|c| *c.borrow_mut() = None);
        let path = dir.join(format!("{name}.png"));
        let ok = capture_rounded(&path, &container, dark);
        heights.push_str(&format!("{name} {:.1}\n", container.frame().size.height));
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_owned()) }
    };
    for (name, setup) in states() {
        shot(name, true, &*setup, None);
    }
    shot("11-hover-header", true, &owner, Some(MenuSection::Background.title()));
    shot("12-hover-row", true, &owner, Some("Open Transcript"));
    shot("13-all-open-light", false, &owner, None);
    // Background Tasks open: a task row under the pointer, and the section in the light look.
    let background = |s: &mut super::State| { owner(s); only(s, MenuSection::Background); };
    shot("18-background-hover", true, &background, Some(BackgroundTask::Goals.title()));
    shot("19-background-light", false, &background, None);
    // The submenus a usability judge follows: Background Tasks ▸ Model (where each task's own picker lives on this
    // branch), a task's picker, and Agent ▸ Model.
    let state = demo_menu(mtm, |s| { owner(s); only(s, MenuSection::Background); });
    let mut sub = |name: &str, menu: Option<objc2::rc::Retained<objc2_app_kit::NSMenu>>| {
        let path = dir.join(format!("{name}.png"));
        let ok = menu.is_some_and(|m| super::offscreen::stack(&path, &m, true, mtm));
        eprintln!("render {} {}", path.display(), if ok { "ok" } else { "FAILED" });
        if !ok { failures.push(name.to_owned()) }
    };
    sub("20-background-model-submenu", state.background_menu.clone());
    sub("21-islands-picker", state.task_pickers.iter().find(|p| p.task == BackgroundTask::Islands).map(|p| p.menu.clone()));
    // Memory ▸ (which row is the whole), dark and light.
    sub("23-memory-submenu", Some(super::menu::memory_item(&super::offscreen::memory_demo(), mtm).submenu().unwrap()));
    let mut state = demo_menu(mtm, owner);
    let target = state.target();
    sub("22-agent-model-submenu", Some(super::menu::agent_model_menu(&mut state, Some(&*target), mtm)));
    let path = dir.join("24-memory-submenu-light.png");
    if !super::offscreen::stack(&path, &super::menu::memory_item(&super::offscreen::memory_demo(), mtm).submenu().unwrap(), false, mtm) { failures.push("24-memory-submenu-light".into()) }
    super::controls::STILL_SPINNERS.with(|c| c.set(false));
    // What a header click costs the main thread in our code (no window: AppKit's own layout comes on top), median of 5.
    for section in [MenuSection::Background, MenuSection::Engine, MenuSection::Decider, MenuSection::Personalisation, MenuSection::Agent] {
        let mut state = demo_menu(mtm, |s| { owner(s); only(s, MenuSection::Agent); });
        state.on_action = Box::new(|_| {});
        state.menu_open = true;
        let mut times: Vec<f64> = (0..5).map(|_| {
            let t = std::time::Instant::now();
            state.toggle_section(section);
            // AppKit's layout of the changed menu, as the open menu's window would take it.
            let _ = state.menu.size();
            let open = t.elapsed().as_secs_f64() * 1000.0;
            state.toggle_section(MenuSection::Agent);
            open
        }).collect();
        times.sort_by(|a, b| a.total_cmp(b));
        heights.push_str(&format!("toggle-{} {:.2} ms\n", section.title(), times[2]));
    }
    std::fs::write(dir.join("menu-climb.txt"), heights).ok();
    failures
}
