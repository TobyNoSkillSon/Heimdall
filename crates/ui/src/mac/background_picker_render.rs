//! `heimdall-ui-demo --background-picker <dir>`: Background Tasks ▸ Model (`background_picker`) driven through the
//! real State, menu builder and controller selectors (no events, no cursor, nothing ordered on screen), with the
//! checks behind it, and renders at real size (2×) beside Agent ▸ Model for comparison:
//! - the chosen model's provider starts open, the others closed; every provider says API, subscription or this Mac;
//! - a provider's disclosure opens and closes its models in place (the same items, the menu not rebuilt);
//! - a model's radio sends `SetBackgroundModel`; its thinking row shows under it, Agent ▸ Model's selector;
//! - a level sends `SetBackgroundEffort` once, and the caption names it ("Claude Haiku 5.5 · Claude code 2 · Low");
//! - T3's instances arriving while the menu is open lay the rows again in the same submenu, open rows kept open;
//! - a disconnected provider says so and its models cannot be chosen.
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSControlStateValueOn, NSMenu, NSMenuItem};
use objc2_foundation::MainThreadMarker;

use super::background_picker::{self as picker, demo_t3};
use super::offscreen::{demo_menu, full_menu, stack};
use crate::model::*;

type Sent = Rc<RefCell<Vec<UiAction>>>;

const CLAUDE_CODE_2: &str = "t3:claudeAgent_claude_code_2";

/// The user's setup on 30 Oct: Anthropic's API (Opus, Sonnet, Haiku added), OpenCode Go, the built-in engine ready, and
/// T3's Claude code 2 and Codex instances.
fn setup(s: &mut super::State) {
    let all = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
    s.apply(UiUpdate::Permissions(all));
    s.api = crate::demo_anthropic_providers(ProviderAuth::Connected);
    // Background Tasks open (one big section open at a time).
    s.settings_value.collapsed = crate::model::MenuSection::ALL.iter().copied().filter(|x| *x != crate::model::MenuSection::Background).collect();
    s.t3_instances = demo_t3();
}

/// The demo menu in that setup, open, installed as the app's state (the selectors reach it), every action kept.
fn install(mtm: MainThreadMarker) -> (Option<super::State>, Sent) {
    let mut state = demo_menu(mtm, setup);
    let sent: Sent = Default::default();
    let log = sent.clone();
    state.on_action = Box::new(move |a| log.borrow_mut().push(a));
    state.menu_open = true;
    (super::STATE.with(|s| s.replace(Some(state))), sent)
}

fn now<R>(f: impl FnOnce(&mut super::State) -> R) -> R {
    super::with_state_now(f).expect("the state is installed")
}

/// The provider rows' titles as drawn (mark, title, grey suffix, reason).
fn provider_titles() -> Vec<String> {
    now(|s| s.background_rows.providers.iter().map(|p| {
        p.item.view().and_then(|v| v.subviews().iter().find_map(|x| x.downcast::<objc2_app_kit::NSTextField>().ok()))
            .map(|f| f.attributedStringValue().string().to_string().trim_start_matches('\u{fffc}').trim().to_string()).unwrap_or_default()
    }).collect())
}

fn provider(id: &str) -> Option<(Retained<objc2_app_kit::NSButton>, Vec<Retained<NSMenuItem>>)> {
    now(|s| {
        let p = s.background_rows.providers.iter().find(|p| p.id == id)?;
        let items = p.models.clone().filter_map(|i| s.background_rows.models[i].0.enclosingMenuItem()).collect();
        Some((p.disclosure.clone(), items))
    })
}

fn radio(provider: &str, id: &str) -> Option<(usize, Retained<objc2_app_kit::NSButton>)> {
    now(|s| s.background_rows.models.iter().position(|(_, p, i)| p == provider && i == id).map(|n| (n, s.background_rows.models[n].0.clone())))
}

fn thinking_shown(index: usize) -> Option<bool> {
    now(|s| s.background_rows.thinking.get(index).and_then(|t| t.as_ref()).map(|t| !t.item.isHidden()))
}

fn checked() -> Vec<(String, String)> {
    now(|s| s.background_rows.models.iter().filter(|(b, ..)| b.state() == NSControlStateValueOn).map(|(_, p, i)| (p.clone(), i.clone())).collect())
}

fn caption() -> String {
    now(|s| s.background_caption.as_ref().map(|c| c.text()).unwrap_or_default())
}

fn live_menu() -> Retained<NSMenu> {
    now(|s| s.background_menu.clone()).expect("Background Tasks ▸ Model")
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--background-picker runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };

    // 1. At rest: Haiku on Anthropic's API, its provider open.
    let (previous, sent) = install(mtm);
    let controller = now(|s| s.controller.clone());
    let titles = provider_titles();
    check("every provider says how it is reached", titles.len() >= 4 && titles.iter().all(|t| t.contains(" API") || t.contains("subscription") || t.contains("Local") || t.contains("this Mac")), format!("{titles:?}"));
    check("T3's instance is named after its subscription", titles.iter().any(|t| t == "Claude Code subscription · Claude code 2") && titles.iter().any(|t| t == "Codex subscription · T3 Code"), format!("{titles:?}"));
    check("rest: Haiku on Anthropic's API is the one choice", checked() == [(BACKGROUND_DEFAULT.0.to_string(), BACKGROUND_DEFAULT.1.to_string())], format!("{:?}", checked()));
    let (anthropic, anthropic_rows) = provider("anthropic").unwrap();
    let (t3, t3_rows) = provider(CLAUDE_CODE_2).unwrap();
    check("rest: the chosen model's provider is open, the others closed", anthropic.state() == NSControlStateValueOn && anthropic_rows.iter().all(|i| !i.isHidden())
        && t3.state() != NSControlStateValueOn && t3_rows.iter().all(|i| i.isHidden()), String::new());
    check("rest: the caption names model and provider", caption() == "Claude Haiku 5.5 · Anthropic API", caption());
    let add = live_menu().itemArray().iter().find(|i| i.title().to_string() == super::menu::BACKGROUND_ADD_MODEL);
    check("Add Model… opens Agent ▸ Model", add.is_some_and(|i| i.action() == Some(objc2::sel!(openAgentModels:))), String::new());
    check("tags index the live rows", now(|s| s.background_rows.models.iter().enumerate().all(|(i, (b, ..))| b.tag() == i as isize)
        && s.background_rows.providers.iter().enumerate().all(|(i, p)| p.disclosure.tag() == i as isize)), String::new());

    // 2. Claude code 2's disclosure: its four models show in place, and hide again.
    let before: Vec<Retained<NSMenuItem>> = live_menu().itemArray().iter().collect();
    unsafe { t3.performClick(None) };
    let after: Vec<Retained<NSMenuItem>> = live_menu().itemArray().iter().collect();
    let same = before.len() == after.len() && before.iter().zip(&after).all(|(a, b)| std::ptr::eq(&**a, &**b));
    check("open: Claude code 2's four models show, in place (the same items)", same && t3_rows.len() == 4 && t3_rows.iter().all(|i| !i.isHidden()), format!("{} → {}", before.len(), after.len()));
    let (sent_any, open_kept) = (sent.borrow().is_empty(), now(|s| s.background_rows.open.get(CLAUDE_CODE_2).copied()));
    check("open: nothing reaches the app; the choice holds for this run", sent_any && open_kept == Some(true), format!("{:?}", sent.borrow()));
    unsafe { t3.performClick(None) };
    check("close: they hide again", t3_rows.iter().all(|i| i.isHidden()), String::new());
    unsafe { t3.performClick(None) };
    shot(dir, "picker-t3-open-api-chosen", true, &mut check);

    // 3. Haiku through Claude code 2: sent, the radios exclusive, its thinking row under it.
    let (n, haiku) = radio(CLAUDE_CODE_2, "claude-haiku-5-5").unwrap();
    let (api_n, _) = radio("anthropic", "claude-haiku-5-5").unwrap();
    check("Haiku's thinking row waits until it is chosen", thinking_shown(n) == Some(false) && thinking_shown(api_n) == Some(true), format!("{:?} {:?}", thinking_shown(n), thinking_shown(api_n)));
    unsafe { haiku.performClick(None) };
    check("choose: sends SetBackgroundModel for the T3 instance", sent.borrow().as_slice() == [UiAction::SetBackgroundModel { provider: CLAUDE_CODE_2.into(), id: "claude-haiku-5-5".into() }], format!("{:?}", sent.borrow()));
    check("choose: only it is checked", checked() == [(CLAUDE_CODE_2.to_string(), "claude-haiku-5-5".to_string())], format!("{:?}", checked()));
    check("choose: its thinking row shows, the old one hides", thinking_shown(n) == Some(true) && thinking_shown(api_n) == Some(false), String::new());
    let control = now(|s| s.background_rows.thinking[n].as_ref().and_then(|t| t.control.clone())).unwrap();
    let labels: Vec<String> = (0..control.segmentCount()).map(|i| control.labelForSegment(i).map(|l| l.to_string()).unwrap_or_default()).collect();
    check("its levels: T3's six, the default (medium) selected", labels == ["Low", "Medium", "High", "XHigh", "Max", "Ultrathink"] && control.selectedSegment() == 1, format!("{labels:?} {}", control.selectedSegment()));
    check("choose: the caption follows", caption() == "Claude Haiku 5.5 · Claude code 2", caption());
    sent.borrow_mut().clear();

    // 4. A level: Low, sent once; the caption names it.
    control.setSelectedSegment(0);
    unsafe { let _: () = msg_send![&*controller, backgroundEffort: &*control]; }
    check("level: sends SetBackgroundEffort", sent.borrow().as_slice() == [UiAction::SetBackgroundEffort { provider: CLAUDE_CODE_2.into(), id: "claude-haiku-5-5".into(), level: "low".into() }], format!("{:?}", sent.borrow()));
    check("level: the caption says it", caption() == "Claude Haiku 5.5 · Claude code 2 · Low", caption());
    unsafe { let _: () = msg_send![&*controller, backgroundEffort: &*control]; }
    check("level: the same level again sends nothing", sent.borrow().len() == 1, format!("{:?}", sent.borrow()));
    now(super::menu::sync_tracking);
    check("level: a sync keeps Low selected", control.selectedSegment() == 0, format!("{}", control.selectedSegment()));
    sent.borrow_mut().clear();
    shot(dir, "picker-claude-code-2-haiku", true, &mut check);
    shot(dir, "picker-claude-code-2-haiku-light", false, &mut check);

    // 5. T3 lists a third instance while the menu is open: the same submenu holds it, Claude code 2 still open.
    let sub = live_menu();
    let mut more = demo_t3();
    more.insert(0, T3Instance { instance: "claudeAgent".into(), name: "Claude".into(), driver: "claudeAgent".into(), models: more[0].models.clone() });
    now(|s| s.apply(UiUpdate::T3Instances(more)));
    let titles = provider_titles();
    check("T3Instances while open: the new instance's row is in the open submenu", std::ptr::eq(&*sub, &*live_menu()) && titles.iter().any(|t| t == "Claude Code subscription · T3 Code"), format!("{titles:?}"));
    let (t3, t3_rows) = provider(CLAUDE_CODE_2).unwrap();
    let in_menu = t3_rows.iter().all(|i| sub.indexOfItem(i) >= 0);
    check("T3Instances while open: Claude code 2 stays open and chosen", t3.state() == NSControlStateValueOn && in_menu && t3_rows.iter().all(|i| !i.isHidden())
        && checked() == [(CLAUDE_CODE_2.to_string(), "claude-haiku-5-5".to_string())], format!("{:?}", checked()));
    let (n, _) = radio(CLAUDE_CODE_2, "claude-haiku-5-5").unwrap();
    check("T3Instances while open: Haiku's level stays Low", now(|s| s.background_rows.thinking[n].as_ref().and_then(|t| t.control.as_ref().map(|c| c.selectedSegment()))) == Some(0), String::new());
    check("T3Instances while open: nothing sent", sent.borrow().is_empty(), format!("{:?}", sent.borrow()));

    // 6. Anthropic's key gone: the row says so, its models cannot be chosen, a press sends nothing.
    now(|s| s.apply(UiUpdate::ApiProviders(crate::demo_anthropic_providers(ProviderAuth::Missing))));
    let titles = provider_titles();
    check("disconnected: Anthropic API says Not connected", titles.iter().any(|t| t == "Anthropic API — Not connected"), format!("{titles:?}"));
    let (_, haiku) = radio("anthropic", "claude-haiku-5-5").unwrap();
    check("disconnected: its Haiku cannot be chosen", !haiku.isEnabled(), String::new());
    unsafe { let _: () = msg_send![&*controller, chooseBackgroundModel: &*haiku]; }
    check("disconnected: a press sends nothing", sent.borrow().is_empty(), format!("{:?}", sent.borrow()));
    now(|s| { let d = s.background_rows.providers.iter().find(|p| p.id == "anthropic").map(|p| p.disclosure.clone()).unwrap(); picker::toggle(s, &d, true) });
    shot(dir, "picker-anthropic-not-connected", true, &mut check);
    super::STATE.with(|s| *s.borrow_mut() = previous);

    // 7. Renders: the picker closed, Anthropic's API open (the default), and Agent ▸ Model beside them.
    let (previous, _) = install(mtm);
    shot_with(dir, "picker-collapsed", true, Some(&[("anthropic", false)]), picker::Scope::Shared, &mut check);
    shot(dir, "picker-anthropic-open", true, &mut check);
    now(|s| s.api_expanded.insert(("anthropic".into(), "claude-haiku-5-5".into())));
    let ok = now(|s| { let target = s.target(); let m = super::menu::agent_model_menu(s, Some(&*target), mtm); stack(&dir.join("agent-model-compare.png"), &m, true, mtm) });
    check("render agent-model-compare.png", ok, "not written".into());
    super::STATE.with(|s| *s.borrow_mut() = previous);
    // The whole menu with that choice, for the caption in place.
    let path = dir.join("menu-background-caption.png");
    let ok = full_menu(&path, true, mtm, |s| {
        setup(s);
        s.settings_value.background_model = (CLAUDE_CODE_2.into(), "claude-haiku-5-5".into());
        s.settings_value.background_efforts = vec![(CLAUDE_CODE_2.into(), "claude-haiku-5-5".into(), "low".into())];
    });
    check("render menu-background-caption.png", ok, "not written".into());
    islands(dir, mtm, &mut check);
    every_task(dir, mtm, &mut check);
    failures
}

/// Luna as the user saved it for the other jobs (30 Oct 07:37): GPT-6 Luna on Sign in with ChatGPT (the Codex
/// subscription), T3 listing Claude (the first account) beside Claude code 2 and Codex.
pub(crate) fn luna_setup(s: &mut super::State) {
    setup(s);
    if let Some(p) = s.api.providers.iter_mut().find(|p| p.id == "openai") { p.auth = ProviderAuth::Connected; }
    s.api.models.push(ApiModelRow { provider: "openai".into(), id: "gpt-6-luna".into(), name: "GPT-6 Luna".into(), added: true, unavailable: None,
        detail: Some(ApiModelDetail { context_steps: vec![128_000, 272_000], context: 272_000, price: None, thinking_levels: ["low", "medium", "high"].map(String::from).to_vec(), thinking: "medium".into() }) });
    let mut t3 = demo_t3();
    t3.insert(0, T3Instance { instance: "claudeAgent".into(), name: "Claude".into(), driver: "claudeAgent".into(), models: t3[0].models.clone() });
    s.t3_instances = t3;
    s.settings_value.background_model = ("openai".into(), "gpt-6-luna".into());
    s.settings_value.task_models = vec![(BackgroundTask::Islands, ISLANDS_DEFAULT.0.into(), ISLANDS_DEFAULT.1.into())];
}

/// The Background Tasks section of the whole menu, as it opens: its header to the separator after it, at the width
/// the real menu takes for its widest row (a model row's title, badge and chevron).
fn section(path: &Path, mtm: MainThreadMarker, setup: impl FnOnce(&mut super::State)) -> bool {
    let state = demo_menu(mtm, setup);
    let items: Vec<Retained<NSMenuItem>> = state.menu.itemArray().iter().collect();
    let Some(from) = items.iter().position(|i| i.title().to_string() == super::menu::BACKGROUND_TITLE) else { return false };
    let to = items.iter().skip(from + 1).position(|i| i.isSeparatorItem()).map_or(items.len(), |n| from + 1 + n);
    let menu = NSMenu::new(mtm);
    let font = super::util::menu_font();
    let mut width = super::offscreen::MENU_WIDTH;
    for item in &items[from..to] {
        if !item.isHidden() && item.view().is_none() {
            let badge = item.badge().and_then(|b| b.stringValue()).map_or(0.0, |b| super::util::text_width(&b.to_string(), &font) + 12.0);
            width = width.max(39.0 + super::util::text_width(&item.title().to_string(), &font) + badge + if item.hasSubmenu() { 34.0 } else { 16.0 });
        }
        state.menu.removeItem(item);
        menu.addItem(item);
    }
    let view = super::offscreen::stacked(&menu, width.ceil(), (6.0, 6.0), true, mtm);
    super::offscreen::capture(path, &view, true)
}

/// Every task's own model: a model row under each toggle naming what it runs, the same scoped
/// picker (Suggestions without T3), `SetTaskModel` per task, and the fallback's sign from `BackgroundRoutes`.
fn sync_values_for_check(s: &super::State) { picker::sync_values(s) }

fn every_task(dir: &Path, mtm: MainThreadMarker, check: &mut dyn FnMut(&str, bool, String)) {
    let mut state = demo_menu(mtm, luna_setup);
    let sent: Sent = Default::default();
    let log = sent.clone();
    state.on_action = Box::new(move |a| log.borrow_mut().push(a));
    state.menu_open = true;
    let previous = super::STATE.with(|s| s.replace(Some(state)));
    let luna_level = now(|s| s.background_rows.models.iter().position(|(_, p, i)| p == "openai" && i == "gpt-6-luna")
        .and_then(|n| s.background_rows.thinking.get(n).and_then(|t| t.as_ref())).and_then(|t| t.control.as_ref().map(|c| (c.selectedSegment(), t.levels.clone()))));
    // Code review: a job with no level of its own runs its default effort, so no level is lit (not the agent's level).
    // High picked, then Default: the empty level goes to the app (each job's own effort again) and nothing is kept.
    let luna_control = now(|s| s.background_rows.models.iter().position(|(_, p, i)| p == "openai" && i == "gpt-6-luna")
        .and_then(|n| s.background_rows.thinking.get(n).and_then(|t| t.as_ref())).and_then(|t| t.control.clone()));
    if let Some(c) = &luna_control {
        let controller = now(|s| s.controller.clone());
        c.setSelectedSegment(c.segmentCount() - 1);
        unsafe { let _: () = msg_send![&*controller, backgroundEffort: &**c]; }
        sent.borrow_mut().clear();
        c.setSelectedSegment(0);
        unsafe { let _: () = msg_send![&*controller, backgroundEffort: &**c]; }
    }
    let kept = now(|s| s.settings_value.background_efforts.iter().any(|(p, i, _)| p == "openai" && i == "gpt-6-luna"));
    check("Luna's Default sends the empty level and keeps none", sent.borrow().as_slice() == [UiAction::SetBackgroundEffort { provider: "openai".into(), id: "gpt-6-luna".into(), level: String::new() }] && !kept, format!("{:?}", sent.borrow()));
    sent.borrow_mut().clear();
    check("the shared picker's Luna shows Default selected until a level is chosen for the jobs", luna_level.as_ref().is_some_and(|(at, levels)| *at >= 0 && levels.get(*at as usize).is_some_and(|l| l.is_empty())), format!("{luna_level:?}"));
    // A task's picker row (in Background Tasks ▸ Model) is titled by the task; its tooltip is what it runs (climb 2).
    let titles = || now(|s| s.task_pickers.iter().map(|p| (p.task, p.item.toolTip().map(|t| t.to_string()).unwrap_or_default())).collect::<Vec<_>>());
    let expect: Vec<(BackgroundTask, String)> = BackgroundTask::ALL.into_iter()
        .map(|t| (t, if t == BackgroundTask::Islands { "Claude Haiku 5.5 · Claude code 2" } else { "GPT-6 Luna · Codex subscription, the default model" }.to_string())).collect();
    check("every task has its model row: Islands on Haiku through Claude code 2, the others on the shared Luna", titles() == expect, format!("{:?}", titles()));
    // Bug note 2026-11-03T17-37-04 and ("too much text"): one line per row, its value the model it
    // really runs as a quiet short name (never "shared" or "Default"); a chosen model after its provider's mark; where it
    // runs is the tooltip. The Default Model row says its model the same way and names the tasks it serves in its tooltip.
    let lines = now(|s| s.task_pickers.iter().map(|p| (p.task, super::menu::value_of(&p.item), super::menu::value_marked(&p.item), super::menu::subtitle_of(&p.item))).collect::<Vec<_>>());
    let want: Vec<_> = BackgroundTask::ALL.into_iter().map(|t| if t == BackgroundTask::Islands { (t, Some("Haiku 5.5".to_string()), true, None) } else { (t, Some("Luna".to_string()), false, None) }).collect();
    check("each task row: one line, its resolved model's short name; its own model after its provider's mark", lines == want, format!("{lines:?}"));
    let default_row = now(|s| s.background_menu.as_ref().and_then(|sub| s.menu.itemArray().iter().find(|i| i.submenu().is_some_and(|m| std::ptr::eq(&*m, &**sub)))));
    let default_row = default_row.map(|i| (super::menu::row_title(&i), super::menu::value_of(&i), super::menu::subtitle_of(&i), i.toolTip().map(|t| t.to_string()).unwrap_or_default()));
    check("the Default Model row: one line, its model's short name; provider and the five tasks it serves in its tooltip",
        default_row.as_ref().is_some_and(|(title, value, line, tip)| title == super::menu::BACKGROUND_MODEL_TITLE && value.as_deref() == Some("Luna")
            && line.is_none() && tip.starts_with("GPT-6 Luna · Codex subscription") && tip.contains("Plan Lists, Goals, Voice Names, Day Timeline and Suggestions")), format!("{default_row:?}"));
    // Code review (climb 3 Nov): the same name with another provider's mark still reaches AppKit.
    if let Some(item) = now(|s| s.background_menu.as_ref().and_then(|sub| s.menu.itemArray().iter().find(|i| i.submenu().is_some_and(|m| std::ptr::eq(&*m, &**sub))))) {
        super::menu::set_value_marked(&item, Some("Luna"), Some("codex"));
        let before = item.attributedTitle().map(|t| &*t as *const _ as usize);
        super::menu::set_value_marked(&item, Some("Luna"), Some("openai"));
        let after = item.attributedTitle().map(|t| &*t as *const _ as usize);
        super::menu::set_value_marked(&item, Some("Luna"), Some("openai"));
        let again = item.attributedTitle().map(|t| &*t as *const _ as usize);
        check("a value's mark changing alone redraws it; nothing changing leaves it", before != after && after == again, format!("{before:?} {after:?} {again:?}"));
        now(|s| sync_values_for_check(s));
    }
    // Climb 3 Nov round 4: each task picker's Use Default Model row names the default after it.
    let back = now(|s| s.task_pickers.iter().map(|p| p.rows.models.first().map(|(b, ..)| b.attributedTitle().string().to_string()).unwrap_or_default()).collect::<Vec<_>>());
    check("each picker's Use Default Model row names the default", back.iter().all(|t| t == &format!("{}  Luna", picker::SAME_AS_SHARED)), format!("{back:?}"));
    // Climb 2: one native row per task in the menu, its switch first in its submenu, then its picker.
    let placed = now(|s| s.task_pickers.iter().all(|p| {
        let toggle = s.background_task_rows.iter().find(|(t, _)| *t == p.task).and_then(|(_, r)| r.enclosingMenuItem());
        unsafe { p.item.menu() }.is_some_and(|m| std::ptr::eq(&*m, &*s.menu)) && super::menu::row_title(&p.item) == p.task.title()
            && toggle.is_some_and(|t| p.menu.indexOfItem(&t) == 0)
    }));
    check("each task is one row of the menu, its switch first in its own submenu", placed, String::new());
    // Suggestions is a tool loop: no T3 instance in its picker, nor a T3 stand-in.
    let suggestions = |f: &dyn Fn(&picker::TaskPicker) -> bool| now(|s| s.task_pickers.iter().find(|p| p.task == BackgroundTask::Suggestions).is_some_and(f));
    check("Suggestions' picker has no T3 rows", suggestions(&|p| !p.rows.shape.is_empty() && p.rows.shape.iter().all(|r| !r.id.starts_with(T3_PROVIDER_PREFIX))
        && p.rows.models.iter().all(|(_, provider, _)| !provider.starts_with(T3_PROVIDER_PREFIX))), String::new());
    check("the others offer T3", now(|s| s.task_pickers.iter().filter(|p| p.task != BackgroundTask::Suggestions).all(|p| p.rows.shape.iter().any(|r| r.id == CLAUDE_CODE_2)))
        && now(|s| s.background_rows.shape.iter().any(|r| r.id == CLAUDE_CODE_2)), String::new());
    // Each task's own pick: SetTaskModel for that task only (Suggestions on Haiku through Anthropic's API).
    for task in BackgroundTask::ALL {
        let (provider, id) = if task == BackgroundTask::Suggestions { ("anthropic", "claude-haiku-5-5") } else { (CLAUDE_CODE_2, "claude-sonnet-5-5") };
        let radio = now(|s| s.task_pickers.iter().find(|p| p.task == task).and_then(|p| p.rows.models.iter().find(|(_, pr, i)| pr == provider && i == id)).map(|(b, ..)| b.clone()));
        let Some(radio) = radio else { check(&format!("{}: its picker lists {provider} {id}", task.title()), false, String::new()); continue };
        sent.borrow_mut().clear();
        unsafe { radio.performClick(None) };
        let name = if task == BackgroundTask::Suggestions { "Claude Haiku 5.5 · Anthropic API" } else { "Claude Sonnet 5.5 · Claude code 2" };
        let row = titles().into_iter().find(|(t, _)| *t == task).map(|(_, x)| x).unwrap_or_default();
        check(&format!("{}: SetTaskModel for it alone, its row follows", task.title()), sent.borrow().as_slice() == [UiAction::SetTaskModel { task, provider: provider.into(), id: id.into() }]
            && row == name && caption() == "GPT-6 Luna · Codex subscription", format!("{:?} {row}", sent.borrow()));
    }
    // The app's settings while the menu is open: the rows follow in place (the same items).
    let items: Vec<Retained<NSMenuItem>> = now(|s| s.task_pickers.iter().map(|p| p.item.clone()).collect());
    let settings = now(|s| Settings { task_models: vec![(BackgroundTask::Islands, ISLANDS_DEFAULT.0.into(), ISLANDS_DEFAULT.1.into())], ..s.settings_value.clone() });
    now(|s| s.apply(UiUpdate::Settings(settings)));
    let kept = now(|s| s.task_pickers.iter().zip(&items).all(|(p, i)| std::ptr::eq(&*p.item, &**i)));
    check("settings while open: the rows follow in place", kept && titles() == expect, format!("{:?}", titles()));
    // The fallback: Islands on Claude (Claude code 2's quota ran out), Plan Lists on Claude code 2; then cleared.
    // The sign is the task row's value while it is on a fallback (climb 2): the arrow and what answers now.
    let badge = |task: BackgroundTask| now(|s| s.task_pickers.iter().find(|p| p.task == task).map(|p| {
        (super::menu::value_of(&p.item).filter(|t| t.starts_with('\u{21aa}')), p.item.toolTip().map(|t| t.to_string()).filter(|t| t.contains(" runs on ")))
    })).unwrap();
    now(|s| s.apply(UiUpdate::BackgroundRoutes(vec![
        (BackgroundTask::Islands, "t3:claudeAgent".into(), "claude-haiku-5-5".into()),
        (BackgroundTask::Plan, CLAUDE_CODE_2.into(), "claude-haiku-5-5".into()),
    ])));
    let (islands, tip) = badge(BackgroundTask::Islands);
    // climb: every row's value is a model, a fallback's too (round 0's judges: "↪ Claude" named a place);
    // where it runs now is the tooltip.
    check("routes: Islands shows ↪ and the model answering now, its tooltip where and the pick it replaced", islands.as_deref() == Some("↪ Haiku 5.5")
        && tip.as_deref().is_some_and(|t| t.contains("Claude Haiku 5.5 · Claude code 2") && t.contains("Claude Haiku 5.5 · Claude")), format!("{islands:?} {tip:?}"));
    check("routes: Plan Lists shows ↪ and what answers now, the others nothing", badge(BackgroundTask::Plan).0.as_deref().is_some_and(|b| b.starts_with("↪ Haiku 5.5"))
        && [BackgroundTask::VoiceNames, BackgroundTask::DayTimeline, BackgroundTask::Suggestions].into_iter().all(|t| badge(t).0.is_none()), String::new());
    let default_tip = now(|s| s.background_menu.as_ref().and_then(|sub| s.menu.itemArray().iter().find(|i| i.submenu().is_some_and(|m| std::ptr::eq(&*m, &**sub)))).and_then(|i| i.toolTip()).map(|t| t.to_string())).unwrap_or_default();
    check("routes: the Default Model no longer serves Plan Lists, which names what answers now", !default_tip.contains("Plan Lists") && default_tip.contains("Goals")
        && badge(BackgroundTask::Plan).0.as_deref() == Some("↪ Haiku 5.5"), format!("{default_tip:?}"));
    now(|s| s.apply(UiUpdate::BackgroundRoutes(vec![])));
    check("routes cleared: no sign, no tooltip", BackgroundTask::ALL.into_iter().all(|t| badge(t) == (None, None)), String::new());
    super::STATE.with(|s| *s.borrow_mut() = previous);

    // Renders: the section with all five rows, the same with the fallback's sign, and Suggestions' picker.
    let path = dir.join("section-every-task.png");
    let ok = section(&path, mtm, luna_setup);
    check(&format!("render {}", path.display()), ok, "not written".into());
    let path = dir.join("section-fallback.png");
    let ok = section(&path, mtm, |s| {
        luna_setup(s);
        s.background_routes = vec![(BackgroundTask::Islands, "t3:claudeAgent".into(), "claude-haiku-5-5".into()), (BackgroundTask::Plan, CLAUDE_CODE_2.into(), "claude-haiku-5-5".into())];
    });
    check(&format!("render {}", path.display()), ok, "not written".into());
    // The whole menu, for its height with every model row.
    let path = dir.join("menu-every-task.png");
    let ok = full_menu(&path, true, mtm, luna_setup);
    check(&format!("render {}", path.display()), ok, "not written".into());
    let state = demo_menu(mtm, luna_setup);
    let path = dir.join("suggestions-picker.png");
    let ok = state.task_pickers.iter().find(|p| p.task == BackgroundTask::Suggestions).is_some_and(|p| {
        let open: BTreeMap<String, bool> = [("openai".to_string(), true)].into();
        let (menu, _) = picker::build(&state.api, &state.engine, &state.t3_instances, &state.settings_value, &open, picker::Scope::Task(p.task), None, mtm);
        stack(&path, &menu, true, mtm)
    });
    check(&format!("render {}", path.display()), ok, "not written".into());
}

/// the island agent on Claude Haiku 5.5 through T3 on Claude code 2, chosen on its own; his saved
/// GPT-6 Luna (T3's Codex) keeps the other jobs. Background Tasks ▸ Islands' own Model row, its picker, `SetTaskModel`.
fn islands(dir: &Path, mtm: MainThreadMarker, check: &mut dyn FnMut(&str, bool, String)) {
    const LUNA: (&str, &str) = ("t3:codex", "gpt-6-luna");
    let (previous, sent) = install(mtm);
    now(|s| { s.settings_value.background_model = (LUNA.0.into(), LUNA.1.into()); s.rebuild_menu(); });
    let task = BackgroundTask::Islands;
    let row = || now(|s| s.task_pickers.iter().find(|p| p.task == task).map(|p| (p.item.clone(), p.item.toolTip().map(|t| t.to_string()).unwrap_or_default())));
    let rows = |f: &dyn Fn(&picker::Rows) -> bool| now(|s| s.task_pickers.iter().find(|p| p.task == task).is_some_and(|p| f(&p.rows)));
    let Some((item, title)) = row() else { check("Islands has its own Model row", false, String::new()); super::STATE.with(|s| *s.borrow_mut() = previous); return };
    let in_menu = now(|s| unsafe { item.menu() }.is_some_and(|m| std::ptr::eq(&*m, &*s.menu)));
    check("Islands is a row of the menu and opens its own picker", in_menu && item.hasSubmenu(), String::new());
    check("no model of its own: the row names the shared model, Use Default Model checked", title.starts_with("GPT-6 Luna · Codex")
        && rows(&|r| r.models[0].1.is_empty() && r.models[0].0.state() == NSControlStateValueOn && r.models.iter().skip(1).all(|(b, ..)| b.state() != NSControlStateValueOn)), title);
    // Claude code 2 opened, then Haiku: the task's own model, the shared one untouched.
    let pick = |provider: &str, id: &str| now(|s| s.task_pickers[0].rows.models.iter().find(|(_, p, i)| p == provider && i == id).map(|(b, ..)| b.clone())).unwrap();
    let disclosure = now(|s| s.task_pickers[0].rows.providers.iter().find(|p| p.id == CLAUDE_CODE_2).map(|p| p.disclosure.clone())).unwrap();
    unsafe { disclosure.performClick(None) };
    unsafe { pick(CLAUDE_CODE_2, "claude-haiku-5-5").performClick(None) };
    check("Haiku on Claude code 2: sends SetTaskModel for Islands", sent.borrow().as_slice() == [UiAction::SetTaskModel { task, provider: CLAUDE_CODE_2.into(), id: "claude-haiku-5-5".into() }], format!("{:?}", sent.borrow()));
    let title = row().unwrap().1;
    check("the Islands row names its model; Background Tasks' caption keeps Luna", title == "Claude Haiku 5.5 · Claude code 2" && caption() == "GPT-6 Luna · Codex", format!("{title} / {}", caption()));
    let haiku_at = now(|s| s.task_pickers[0].rows.models.iter().position(|(_, p, i)| p == CLAUDE_CODE_2 && i == "claude-haiku-5-5")).unwrap();
    check("only Haiku is checked in the Islands picker, its thinking row shown", rows(&|r| r.models.iter().filter(|(b, ..)| b.state() == NSControlStateValueOn).count() == 1
        && r.models[haiku_at].0.state() == NSControlStateValueOn && r.thinking[haiku_at].as_ref().is_some_and(|t| !t.item.isHidden())), String::new());
    check("the shared picker still has Luna", checked() == [(LUNA.0.to_string(), LUNA.1.to_string())], format!("{:?}", checked()));
    unsafe { pick(CLAUDE_CODE_2, "claude-haiku-5-5").performClick(None) };
    check("Haiku again sends nothing", sent.borrow().len() == 1, format!("{:?}", sent.borrow()));
    shot_with(dir, "islands-model-haiku-claude-code-2", true, None, picker::Scope::Task(task), check);
    let path = dir.join("menu-islands-model.png");
    let ok = full_menu(&path, true, mtm, |s| {
        setup(s);
        s.settings_value.background_model = (LUNA.0.into(), LUNA.1.into());
        s.settings_value.task_models = vec![(task, ISLANDS_DEFAULT.0.into(), ISLANDS_DEFAULT.1.into())];
    });
    check("render menu-islands-model.png", ok, "not written".into());
    sent.borrow_mut().clear();
    // Its level is the model's, through the same control.
    let control = now(|s| s.task_pickers[0].rows.thinking[haiku_at].as_ref().and_then(|t| t.control.clone())).unwrap();
    control.setSelectedSegment(0);
    let controller = now(|s| s.controller.clone());
    unsafe { let _: () = objc2::msg_send![&*controller, backgroundEffort: &*control]; }
    check("its level sends SetBackgroundEffort; the row says it", sent.borrow().as_slice() == [UiAction::SetBackgroundEffort { provider: CLAUDE_CODE_2.into(), id: "claude-haiku-5-5".into(), level: "low".into() }]
        && row().unwrap().1 == "Claude Haiku 5.5 · Claude code 2 · Low", format!("{:?} {}", sent.borrow(), row().unwrap().1));
    sent.borrow_mut().clear();
    // Its row names its own model's short name after its mark; off: "Off"; on again, the name is back. One line throughout.
    let value = || (super::menu::value_of(&item), super::menu::value_marked(&item), super::menu::subtitle_of(&item));
    super::check_row_clicked(super::menu::background_task_row(task));
    let off = value();
    super::check_row_clicked(super::menu::background_task_row(task));
    let st = value();
    check("Islands' row names its own model; Off while it is off; one line",
        st == (Some("Haiku 5.5".into()), true, None) && off == (Some(picker::OFF.into()), false, None), format!("{st:?} {off:?}"));
    sent.borrow_mut().clear();
    // Back to the shared model.
    unsafe { now(|s| s.task_pickers[0].rows.models[0].0.clone()).performClick(None) };
    check("Use Default Model: SetTaskModel with no provider; the row names Luna again", sent.borrow().as_slice() == [UiAction::SetTaskModel { task, provider: String::new(), id: String::new() }]
        && row().unwrap().1.starts_with("GPT-6 Luna · Codex"), format!("{:?} {}", sent.borrow(), row().unwrap().1));
    sent.borrow_mut().clear();
    // The app's settings name Islands' default (`ISLANDS_DEFAULT`): the open menu follows, nothing sent back.
    let settings = now(|s| Settings { task_models: vec![(task, ISLANDS_DEFAULT.0.into(), ISLANDS_DEFAULT.1.into())], ..s.settings_value.clone() });
    now(|s| s.apply(UiUpdate::Settings(settings)));
    check("Settings with Islands' default: the row and its radio follow", row().unwrap().1.starts_with("Claude Haiku 5.5 · Claude code 2")
        && rows(&|r| r.models[0].0.state() != NSControlStateValueOn) && sent.borrow().is_empty(), format!("{} {:?}", row().unwrap().1, sent.borrow()));
    super::STATE.with(|s| *s.borrow_mut() = previous);
}

/// Background Tasks ▸ Model drawn from the installed state, a fresh copy (a capture takes the rows' views out of their
/// menu), its providers open as the live picker has them.
fn shot(dir: &Path, name: &str, dark: bool, check: &mut dyn FnMut(&str, bool, String)) {
    shot_with(dir, name, dark, None, picker::Scope::Shared, check)
}

fn shot_with(dir: &Path, name: &str, dark: bool, open: Option<&[(&str, bool)]>, scope: picker::Scope, check: &mut dyn FnMut(&str, bool, String)) {
    let mtm = MainThreadMarker::new().unwrap();
    let path = dir.join(format!("{name}.png"));
    let ok = now(|s| {
        let rows = match scope { picker::Scope::Shared => &s.background_rows, picker::Scope::Task(t) => &s.task_pickers.iter().find(|p| p.task == t).expect("the task's picker").rows };
        let mut live: BTreeMap<String, bool> = rows.providers.iter().map(|p| (p.id.clone(), p.disclosure.state() == NSControlStateValueOn)).collect();
        for (id, on) in open.unwrap_or_default() { live.insert(id.to_string(), *on); }
        let (menu, _) = picker::build(&s.api, &s.engine, &s.t3_instances, &s.settings_value, &live, scope, None, mtm);
        stack(&path, &menu, dark, mtm)
    });
    check(&format!("render {}", path.display()), ok, "not written".into());
}
