//! `heimdall-ui-demo --engine-models <dir>`: the Engine section's models (30 Oct: moved out of Agent ▸ Model) with every
//! model and precision of the Splash plan (28 Oct, `Assistant/Plans/heimdall-engine-models`): six models, 24 precisions
//! from their uncensored repos, the engine's 48 GB ceiling. Driven through the real State and rendered offscreen at 2×
//! (nothing on screen), with checks:
//! - the quick-pick lists only the precisions on this Mac (and the one downloading), the loaded one resting lit;
//! - a quick-pick click loads that precision and never touches the agent's model;
//! - Add Model lists every model; a model opens to its precisions, the giants too, those over the ceiling warning
//!   with their RAM; a precision offers its download (size first) or its trash; a download shows progress with a stop;
//! - a click on a precision's name downloads it; nothing downloads unless one is pressed;
//! - the trash asks once (a red Delete pill), the second press deletes; closing the menu disarms it;
//! - Agent ▸ Model's This Mac is the engine alone: no model, precision or download rows; Providers ▸ Local Splash
//!   Engine lists no model radios.
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSMenu};
use objc2_foundation::MainThreadMarker;

use super::engine_rows::Trailing;
use super::offscreen::{demo_menu, stack};
use super::Press;
use crate::model::{EngineDownload, EngineModel, EngineQuant, EngineState, EngineStatus, UiAction, UiUpdate};

/// The engine's memory ceiling, 48 GiB (`max_memory`).
const CEILING: u64 = 48 << 30;

fn q(quant: &str, source: &str, size: u64, ram: u64) -> EngineQuant {
    EngineQuant { quant: quant.into(), source: source.into(), size_bytes: Some(size), ram_bytes: Some(ram), downloaded: false }
}

fn model(id: &str, label: &str, quant: &str, context: u64, quants: Vec<EngineQuant>) -> EngineModel {
    let source = quants.iter().find(|x| x.quant == quant).map(|x| x.source.clone()).unwrap_or_default();
    EngineModel { id: id.into(), label: label.into(), quant: quant.into(), source, quants, context: Some(context), download: None }
}

/// The plan's catalog (sizes measured from Hugging Face metadata; RAM derived: weights, projector, draft, int8 KV at
/// 262,144 tokens, 1 GB).
pub(crate) fn splash_catalog() -> Vec<EngineModel> {
    vec![
        model("qwen3.8-27b", "Qwen3.8 27B", "UD-Q4_K_XL", 262144, vec![
            q("UD-Q2_K_XL", "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q2_K_XL", 10011909024, 24382102508),
            q("UD-Q3_K_XL", "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q3_K_XL", 13326125984, 27696319468),
            q("UD-Q4_K_XL", "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q4_K_XL", 17378626464, 31748819948),
            q("UD-Q5_K_XL", "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q5_K_XL", 20673612704, 35043806188),
            q("UD-Q6_K_XL", "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q6_K_XL", 24826588064, 39196781548),
            q("Q8_0", "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:Q8_0", 29047084320, 43417277804),
        ]),
        model("qwen3.6-35b-a3b", "Qwen3.6 35B A3B", "Q4_K", 262144, vec![
            q("Q2_K", "huihui-ai/Huihui-Qwen3.6-35B-A3B-abliterated-MTP-GGUF:Q2_K", 13246129376, 18882586845),
            q("Q3_K", "huihui-ai/Huihui-Qwen3.6-35B-A3B-abliterated-MTP-GGUF:Q3_K", 17165606112, 22802063581),
            q("Q4_K", "huihui-ai/Huihui-Qwen3.6-35B-A3B-abliterated-MTP-GGUF:Q4_K", 21712409824, 27348867293),
            q("Q5_K", "huihui-ai/Huihui-Qwen3.6-35B-A3B-abliterated-MTP-GGUF:Q5_K", 25346479328, 30982936797),
            q("Q6_K", "huihui-ai/Huihui-Qwen3.6-35B-A3B-abliterated-MTP-GGUF:Q6_K", 29207678176, 34844135645),
            q("Q8_0", "huihui-ai/Huihui-Qwen3.6-35B-A3B-abliterated-MTP-GGUF:Q8_0", 37801096416, 43437553885),
        ]),
        model("flint", "Flint 40B A2.5B", "Q4_K_M", 262144, vec![
            q("IQ2_M", "dima6312/Flint-40B-A2.5B-abliterated-GGUF:IQ2_M", 13106540448, 18670068569),
            q("Q3_K_M", "dima6312/Flint-40B-A2.5B-abliterated-GGUF:Q3_K_M", 19100276640, 24663804761),
            q("Q4_K_M", "dima6312/Flint-40B-A2.5B-abliterated-GGUF:Q4_K_M", 24166842272, 29730370393),
            q("Q5_K_M", "dima6312/Flint-40B-A2.5B-abliterated-GGUF:Q5_K_M", 28314255264, 33877783385),
            q("Q6_K", "dima6312/Flint-40B-A2.5B-abliterated-GGUF:Q6_K", 32720881568, 38284409689),
            q("Q8_0", "dima6312/Flint-40B-A2.5B-abliterated-GGUF:Q8_0", 42350359360, 47913887481),
        ]),
        model("ternary-bonsai-2-27b", "Ternary Bonsai 2 27B", "PQ2_0", 262144, vec![
            q("PQ2_0", "OS-Software/Ternary-Bonsai-2-27B-Uncensored-Heretic-v2-GGUF:PQ2_0", 7206168928, 21576362380),
        ]),
        model("ornith-1.5-122b", "Ornith 1.5 122B", "IQ2_XXS-UNCENSORED", 262144, vec![
            q("IQ2_XXS-UNCENSORED", "dealignai/Ornith-1.5-122B-UNCENSORED-GGUF:IQ2_XXS-UNCENSORED", 44072673440, 51063492573),
            q("Q4_K_M-UNCENSORED", "dealignai/Ornith-1.5-122B-UNCENSORED-GGUF:Q4_K_M-UNCENSORED", 79371735200, 86362554333),
        ]),
        model("qwen3.8-flash-next", "Qwen3.8 Flash Next", "IQ2_XS", 262144, vec![
            q("IQ2_XS", "SC117/Qwen3.8-Flash-Next-GSQ-RCO-abliterated-GGUF:IQ2_XS", 68465153504, 76410972414),
            q("IQ3_XXS", "SC117/Qwen3.8-Flash-Next-GSQ-RCO-abliterated-GGUF:IQ3_XXS", 76142283328, 84088102238),
            q("IQ3_S", "SC117/Qwen3.8-Flash-Next-GSQ-RCO-abliterated-GGUF:IQ3_S", 83863586496, 91809405406),
        ]),
    ]
}

/// The engine as the user would meet it: Qwen3.8 27B at UD-Q4_K_XL serving, Q8_0 also on disk, UD-Q6_K_XL downloading.
fn engine() -> EngineStatus {
    let mut models = splash_catalog();
    let qwen = &mut models[0];
    for x in &mut qwen.quants { x.downloaded = x.quant == "UD-Q4_K_XL" || x.quant == "Q8_0"; }
    qwen.download = Some(EngineDownload { quant: "UD-Q6_K_XL".into(), done_bytes: 10_400_000_000, total_bytes: Some(24_826_588_064), error: None });
    EngineStatus { state: EngineState::Ready, model_id: "qwen3.8-27b".into(), model_label: "Qwen3.8 27B".into(), models, port: 8100,
        context_limit: Some(262_144), cache_used_bytes: None, cache_limit_bytes: None, memory_limit_bytes: Some(CEILING) }
}

type Sent = Rc<RefCell<Vec<UiAction>>>;

fn open_state(mtm: MainThreadMarker) -> (super::State, Sent) {
    let mut state = demo_menu(mtm, |s| {
        let all = crate::Permission::ALL.iter().map(|p| (*p, crate::PermissionState::Granted)).collect();
        s.apply(UiUpdate::Permissions(all));
        s.api = crate::ApiProviders { selected: None, ..crate::demo_anthropic_providers(crate::ProviderAuth::Connected) };
        s.engine = engine();
        s.settings_value.engine_added = true;
    });
    let sent: Sent = Default::default();
    let log = sent.clone();
    state.on_action = Box::new(move |a| log.borrow_mut().push(a));
    state.menu_open = true;
    (state, sent)
}

fn index(state: &super::State, id: &str, quant: &str) -> usize {
    state.engine_rows.precisions.iter().position(|p| p.id == id && p.quant == quant).expect("precision row")
}

/// Add Model ▸ `id`'s submenu as the open menu holds it.
fn model_menu(state: &super::State, id: &str) -> Option<objc2::rc::Retained<NSMenu>> {
    state.engine_rows.models.iter().find(|(_, m)| m == id).and_then(|(i, _)| i.submenu())
}

/// A render of `menu` at the real menu's width (a section's rows lay out as wide as the menu they sit in).
pub(crate) fn shoot_wide(path: &Path, menu: &NSMenu, mtm: MainThreadMarker) -> bool {
    let container = super::offscreen::stacked(menu, super::offscreen::MENU_WIDTH, (6.0, 6.0), true, mtm);
    super::offscreen::capture(path, &container, true)
}

/// The Engine section's rows (header to Add Model) of the main menu, as a menu of their own for a render.
pub(crate) fn section_menu(state: &super::State, mtm: MainThreadMarker) -> objc2::rc::Retained<NSMenu> {
    let menu = NSMenu::new(mtm);
    let Some((header, items)) = state.sections.get(&crate::model::MenuSection::Engine) else { return menu };
    let all: Vec<_> = header.enclosingMenuItem().into_iter().chain(items.iter().cloned()).collect();
    for item in all {
        if let Some(parent) = unsafe { item.menu() } { parent.removeItem(&item); }
        menu.addItem(&item);
    }
    menu
}

pub(crate) fn render(dir: &Path) -> Vec<String> {
    let mtm = MainThreadMarker::new().expect("--engine-models runs on the main thread");
    NSApplication::sharedApplication(mtm).setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    std::fs::create_dir_all(dir).ok();
    super::controls::STILL_SPINNERS.with(|c| c.set(true));
    let mut failures = vec![];
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("{} {name}{}", if ok { "ok  " } else { "FAIL" }, if ok { String::new() } else { format!(": {detail}") });
        if !ok { failures.push(name.to_owned()) }
    };
    let shoot = |name: &str, menu: &NSMenu, check: &mut dyn FnMut(&str, bool, String)| {
        let path = dir.join(format!("{name}.png"));
        let ok = stack(&path, menu, true, mtm);
        check(&format!("render {}", path.display()), ok, "not written".into());
    };

    // 1. The section: Qwen at UD-Q4_K_XL loaded, Q8_0 also here, UD-Q6_K_XL on its way.
    let (mut state, sent) = open_state(mtm);
    let picks: Vec<(String, String)> = state.engine_rows.picks.iter().map(|p| (p.entry.id.clone(), p.entry.quant.clone())).collect();
    let want: Vec<(String, String)> = [("qwen3.8-27b", "UD-Q4_K_XL"), ("qwen3.8-27b", "UD-Q6_K_XL"), ("qwen3.8-27b", "Q8_0")].map(|(a, b)| (a.to_string(), b.to_string())).to_vec();
    check("the quick-pick lists only what is on this Mac, and the download on its way", picks == want, format!("{picks:?}"));
    let lit: Vec<&str> = state.engine_rows.picks.iter().filter(|p| p.row.is_resting()).map(|p| p.entry.quant.as_str()).collect();
    check("the loaded precision rests lit, alone", lit == ["UD-Q4_K_XL"], format!("{lit:?}"));
    let downloading = state.engine_rows.picks.iter().find(|p| p.entry.quant == "UD-Q6_K_XL").unwrap();
    check("the download shows inline: greyed, a bar at 42%", !downloading.item.isEnabled() && downloading.bar.as_ref().is_some_and(|b| (b.doubleValue() - 0.4189).abs() < 0.01), String::new());
    check("nothing is sent at rest", sent.borrow().is_empty(), format!("{:?}", sent.borrow()));
    let path = dir.join("engine-section-ready.png");
    let ok = shoot_wide(&path, &section_menu(&state, mtm), mtm);
    check(&format!("render {}", path.display()), ok, String::new());

    // 2. A quick-pick click loads that precision; the agent's model is not touched.
    state.rebuild_menu();
    let q8 = state.engine_rows.picks.iter().position(|p| p.entry.quant == "Q8_0").unwrap();
    state.load_pick(q8);
    check("a quick-pick click loads it, and only that", sent.borrow().as_slice() == [UiAction::LoadEngineModel { id: "qwen3.8-27b".into(), quant: "Q8_0".into() }], format!("{:?}", sent.borrow()));
    check("it shows as the engine's at once, on its way", state.engine.state == EngineState::Starting && state.engine.models[0].quant == "Q8_0", format!("{:?}", state.engine.state));
    sent.borrow_mut().clear();

    // 3. Add Model: every model; Qwen's precisions: two on this Mac (trash), one downloading, the rest a download.
    let (mut state, sent) = open_state(mtm);
    check("Add Model lists all six models", state.engine_rows.models.len() == 6 && state.engine_rows.precisions.len() == 24, format!("{} / {}", state.engine_rows.models.len(), state.engine_rows.precisions.len()));
    if let Some(add) = state.engine_rows.add_model.as_ref().and_then(|i| i.submenu()) { shoot("engine-add-model", &add, &mut check); }
    let qwen: Vec<&super::engine_rows::PrecisionRow> = state.engine_rows.precisions.iter().filter(|p| p.id == "qwen3.8-27b").collect();
    let by = |quant: &str| qwen.iter().find(|p| p.quant == quant).unwrap();
    check("on disk: a trash", by("UD-Q4_K_XL").trailing == Trailing::Trash && by("Q8_0").trailing == Trailing::Trash, String::new());
    check("downloading: a bar at 42% with a stop", by("UD-Q6_K_XL").progress.as_ref().is_some_and(|b| (b.doubleValue() - 0.4189).abs() < 0.01), String::new());
    check("not on disk: a download, size first", by("UD-Q2_K_XL").trailing == Trailing::Download(Some(10_011_909_024)), format!("{:?}", by("UD-Q2_K_XL").trailing));
    let badge = state.engine_rows.models.iter().find(|(_, m)| m == "qwen3.8-27b").and_then(|(i, _)| i.badge()).and_then(|b| b.stringValue()).map(|s| s.to_string());
    check("a model downloading says how far on its row", badge.as_deref() == Some("41%"), format!("{badge:?}"));
    if let Some(m) = model_menu(&state, "qwen3.8-27b") { shoot("engine-add-model-qwen", &m, &mut check); }

    // 4. A download pressed (its arrow, or its name): that precision, nothing selected or loaded.
    let i = index(&state, "qwen3.8-27b", "UD-Q2_K_XL");
    state.precision(i, Press::Download);
    check("download: sends that precision only", sent.borrow().as_slice() == [UiAction::DownloadEngineModel { id: "qwen3.8-27b".into(), quant: "UD-Q2_K_XL".into(), then_select: false }], format!("{:?}", sent.borrow()));
    // The name presses the download too (its action, the same row's tag); a name on this Mac presses nothing.
    let name = &state.engine_rows.precisions[i].title;
    check("a name not on this Mac downloads it", name.action() == Some(objc2::sel!(downloadPrecision:)) && name.tag() == i as isize, String::new());
    let on_disk = &state.engine_rows.precisions[index(&state, "qwen3.8-27b", "Q8_0")].title;
    check("a name on this Mac does nothing", on_disk.action().is_none(), String::new());
    sent.borrow_mut().clear();
    let i = index(&state, "qwen3.8-27b", "UD-Q6_K_XL");
    state.precision(i, Press::Stop);
    check("stop: cancels the model's download", sent.borrow().as_slice() == [UiAction::CancelEngineDownload { id: "qwen3.8-27b".into() }], format!("{:?}", sent.borrow()));
    sent.borrow_mut().clear();

    // 5. The trash asks once: the red Delete pill with the size; the second press deletes; closing disarms.
    let i = index(&state, "qwen3.8-27b", "Q8_0");
    state.precision(i, Press::Trash);
    let i = index(&state, "qwen3.8-27b", "Q8_0");
    check("trash once: nothing sent, the red Delete pill shows", sent.borrow().is_empty() && state.engine_rows.precisions[i].trailing == Trailing::ConfirmDelete(Some(29_047_084_320)), format!("{:?}", state.engine_rows.precisions[i].trailing));
    if let Some(m) = model_menu(&state, "qwen3.8-27b") { shoot("engine-add-model-trash-armed", &m, &mut check); }
    state.precision(i, Press::Delete);
    check("delete: sends that precision", sent.borrow().as_slice() == [UiAction::DeleteEngineModel { id: "qwen3.8-27b".into(), quant: "Q8_0".into() }], format!("{:?}", sent.borrow()));
    let i = index(&state, "qwen3.8-27b", "Q8_0");
    check("after delete: the pill is gone", state.engine_rows.precisions[i].trailing == Trailing::Trash, String::new());
    sent.borrow_mut().clear();
    state.precision(index(&state, "qwen3.8-27b", "Q8_0"), Press::Trash);
    state.api_expanded.retain(|(k, _)| k != super::menu::TRASH_ARMED); // menuDidClose:
    state.rebuild_menu();
    let i = index(&state, "qwen3.8-27b", "Q8_0");
    check("closing the menu disarms the trash", state.engine_rows.precisions[i].trailing == Trailing::Trash, String::new());

    // 6. The giants: listed, each precision over the ceiling warning with its RAM, and still downloadable.
    let over: Vec<String> = state.engine_rows.precisions.iter().filter(|p| state.engine.models.iter().find(|m| m.id == p.id)
        .and_then(|m| m.quants.iter().find(|x| x.quant == p.quant)).is_some_and(|x| super::engine_rows::over_limit(x, Some(CEILING)).is_some()))
        .map(|p| format!("{}:{}", p.id, p.quant)).collect();
    let expected: Vec<String> = splash_catalog().iter().flat_map(|m| m.quants.iter().filter(|x| x.ram_bytes.unwrap() > CEILING).map(move |x| format!("{}:{}", m.id, x.quant))).collect();
    check(&format!("over the 48 GB ceiling, listed and warned: {}", over.len()), over == expected && !over.is_empty(), format!("{over:?} vs {expected:?}"));
    if let Some(m) = model_menu(&state, "qwen3.8-flash-next") { shoot("engine-add-model-giant", &m, &mut check); }
    let i = index(&state, "qwen3.8-flash-next", "IQ3_S");
    state.precision(i, Press::Download);
    check("over the ceiling: its download still downloads", sent.borrow().last() == Some(&UiAction::DownloadEngineModel { id: "qwen3.8-flash-next".into(), quant: "IQ3_S".into(), then_select: false }), String::new());

    // 7. A failed download says why and offers the download again; it is no quick-pick row.
    let (mut state, _) = open_state(mtm);
    if let Some(m) = state.engine.models.iter_mut().find(|m| m.id == "flint") {
        m.download = Some(EngineDownload { quant: "Q4_K_M".into(), done_bytes: 0, total_bytes: None, error: Some("Hugging Face unreachable".into()) });
    }
    state.rebuild_menu();
    let i = index(&state, "flint", "Q4_K_M");
    check("a failed download says why", matches!(state.engine_rows.precisions[i].trailing, Trailing::Failed(..)) && state.engine_rows.picks.iter().all(|p| p.entry.id != "flint"), String::new());
    if let Some(m) = model_menu(&state, "flint") { shoot("engine-add-model-download-failed", &m, &mut check); }

    // 8. Agent ▸ Model ▸ This Mac: the engine alone; Providers ▸ Local Splash Engine: no model radios, Remove last.
    let target = state.target();
    let agent = super::menu::agent_model_menu(&mut state, Some(&*target), mtm);
    let titles: Vec<String> = agent.itemArray().iter().map(|x| x.title().to_string()).collect();
    let engine_rows = titles.iter().filter(|t| splash_catalog().iter().any(|m| t.contains(&m.label))).count();
    check("Agent ▸ Model's This Mac is the engine alone: no model, precision or download rows", titles.iter().any(|t| t == super::menu::BUILT_IN_ENGINE) && engine_rows == 0
        && state.api_rows.local.is_some(), format!("{titles:?}"));
    let engine_sub = agent.itemArray().iter().find(|x| x.title().to_string() == super::menu::BUILT_IN_ENGINE && x.hasSubmenu()).and_then(|x| x.submenu()).unwrap();
    let titles: Vec<String> = engine_sub.itemArray().iter().map(|x| x.title().to_string()).collect();
    check("Providers ▸ Local Splash Engine: no model radios", !titles.iter().any(|t| t == "Model") && state.engine_buttons.is_empty() && titles.contains(&"Remove".to_string()), format!("{titles:?}"));
    shoot("engine-provider-submenu", &engine_sub, &mut check);
    super::controls::STILL_SPINNERS.with(|c| c.set(false));
    failures
}
