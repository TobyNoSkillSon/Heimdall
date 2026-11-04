//! Background Tasks ▸ Model ("must work exactly like the main agent's: providers and models expand and
//! collapse in place, with the reasoning/thinking level selectable per model. Allow API-key models too (e.g. Haiku on
//! spare Anthropic API credits)"). One level of rows that open in place: each provider, named with how Heimdall
//! reaches it (API, subscription, this Mac), opens to its models; a model is a radio (no Confirm: a background
//! choice is cheap to undo and runs nothing at once), and the chosen one shows its thinking selector under it, Agent
//! ▸ Model's own control. Providers: the built-in engine and the own engine (On This Mac), then the T3 instances
//! (`UiUpdate::T3Instances`: Claude Code and Codex subscriptions, one short T3 thread a call), the subscriptions and
//! API keys the app connected (`ApiProviders`: added models, Haiku on Anthropic's API always). A provider that cannot
//! run says why on its row and its models cannot be chosen. Add Model… opens Agent ▸ Model.
//!
//! The picker's content is computed first (`providers`, plain data, unit-tested); the rows are drawn from it
//! (`build`), and a change of content while the menu is open lays the rows again in place (`sync`).

use std::collections::BTreeMap;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSButton, NSControlStateValueOff, NSControlStateValueOn, NSMenu, NSMenuItem, NSSegmentedControl};
use objc2_foundation::MainThreadMarker;

use super::controls;
use super::menu;
use super::util::*;
use super::State;
use crate::model::*;

/// How Heimdall reaches a provider, as its row says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Access {
    /// Billed per call on a pasted key (Anthropic API, OpenRouter, a custom API).
    Api,
    /// A plan signed in to (Codex via Sign in with ChatGPT, OpenCode Go, Claude Code in T3).
    Subscription,
    /// The built-in engine and the user's own engine.
    ThisMac,
}

impl Access {
    /// The words a provider's title carries for it.
    pub(crate) fn label(self) -> &'static str {
        match self { Access::Api => "API", Access::Subscription => "subscription", Access::ThisMac => "this Mac" }
    }
}

/// One provider row and the models it opens to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Provider {
    /// The provider id a choice carries (`LOCAL_ENGINE`, "anthropic", "t3:claudeAgent_claude_code_2").
    pub id: String,
    /// "Anthropic API", "Claude Code subscription", "Local Splash Engine".
    pub title: String,
    /// In grey after the title: which T3 instance ("Claude code 2"), or "this Mac" for the own engine.
    pub via: Option<String>,
    /// The mark (`menu::provider_image`, else a symbol).
    pub icon: String,
    pub access: Access,
    /// Why it cannot run now ("Not connected", "Credits used up"); its models cannot be chosen then.
    pub reason: Option<String>,
    pub models: Vec<Model>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Model {
    pub id: String,
    pub name: String,
    /// Its thinking levels, least first, in the provider's names ("low" … "ultrathink"); empty: none to pick.
    pub levels: Vec<String>,
    /// What runs without a choice: T3's own default effort; `None` for API and local models (the job decides).
    pub default_level: Option<String>,
    /// Why this model cannot run when its provider can.
    pub reason: Option<String>,
}

/// Where a model row's radio sits: under its provider's title (Agent ▸ Model's `ENGINE_FACT_X`, its relay models).
const MODEL_X: f64 = 32.0;
/// A model row's title, after its radio.
const MODEL_TITLE_X: f64 = MODEL_X + 20.0;
/// Agent ▸ Model's disclosure column, from the row's right edge (`menu::DISCLOSURE_INSET`).
const DISCLOSURE_INSET: f64 = 30.0;
/// A provider's title, in the native rows' title column (Add Model…), its mark centred in their icon column.
const PROVIDER_X: f64 = 39.0;
const ICON_X: f64 = 22.25;
const THINKING: &str = "Thinking";

/// A T3 instance's provider title, mark and grey suffix by its driver: Claude Code or Codex subscription, then the
/// instance's own name ("Claude code 2"), or "T3 Code" when the title already says it ("Codex").
fn t3_title(i: &T3Instance) -> (String, String, String) {
    let (title, icon) = match i.driver.as_str() {
        "claudeAgent" => (provider_title("claude", "Claude Code", true), "anthropic"),
        "codex" => (provider_title("openai", "Codex", true), "codex"),
        _ => (provider_title("", &i.name, true), "network"),
    };
    let via = if title.to_lowercase().contains(&i.name.to_lowercase()) { "T3 Code".to_string() } else { i.name.clone() };
    (title, icon.into(), via)
}

/// A provider's own problem inside its own row ("OpenCode Go: access rejected" → "access rejected").
fn without_name(problem: &str, name: &str) -> String {
    problem.strip_prefix(name).and_then(|p| p.strip_prefix(": ")).unwrap_or(problem).to_owned()
}

/// `t3:` alone stands for the default instance: the id its row carries.
fn normal(provider: &str) -> String {
    t3_instance(provider).map_or_else(|| provider.to_owned(), |i| format!("{T3_PROVIDER_PREFIX}{i}"))
}

/// The picker's providers and models, in order: On This Mac (the built-in engine's model, the own engine's), then
/// Cloud (T3's instances, the connected subscriptions, the API keys). Anthropic's API is always there with Claude
/// Haiku 5.5 (the default, for credits without a plan); a provider the choice names stays listed while it is gone
/// (its reason says so). A connected provider without a model is left out (Add Model… adds one).
pub(crate) fn providers(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], selected: &(String, String)) -> Vec<Provider> {
    let chosen = (normal(&selected.0), selected.1.clone());
    let mut out = vec![];
    if let Some(m) = engine.models.iter().find(|m| m.id == engine.model_id).or(engine.models.first()) {
        out.push(Provider { id: LOCAL_ENGINE.into(), title: LOCAL_ENGINE_TITLE.into(), via: None, icon: "local".into(), access: Access::ThisMac, reason: None,
            models: vec![Model { id: engine.model_id.clone(), name: EngineStatus::model_title(m), levels: LOCAL_THINKING.map(String::from).to_vec(), default_level: None, reason: None }] });
    }
    let api_row = |p: &ApiProviderRow| -> Option<Provider> {
        let listed = p.auth != ProviderAuth::Missing || p.id == BACKGROUND_DEFAULT.0 || p.id == chosen.0;
        if !listed { return None; }
        let mut models: Vec<Model> = api.models.iter().filter(|m| m.provider == p.id).map(|m| Model {
            id: m.id.clone(), name: m.name.clone(),
            // No level lit until one is chosen here: a job with no level runs its own default effort, not the agent's
            // level (code review, climb 2).
            levels: m.detail.as_ref().map(|d| d.thinking_levels.clone()).unwrap_or_default(), default_level: None, reason: m.unavailable.clone(),
        }).collect();
        let mut keep = |id: &str, fallback: &str| {
            if models.iter().any(|m| m.id == id) { return; }
            let name = api.catalog.iter().find(|r| r.provider == p.id && r.id == id).map_or_else(|| fallback.to_owned(), |r| r.name.clone());
            models.push(Model { id: id.into(), name, levels: vec![], default_level: None, reason: None });
        };
        if p.id == BACKGROUND_DEFAULT.0 { keep(BACKGROUND_DEFAULT.1, "Claude Haiku 5.5"); }
        if chosen.0 == p.id { keep(&chosen.1, &chosen.1); }
        if models.is_empty() { return None; }
        let reason = match &p.auth {
            ProviderAuth::Connected => None,
            ProviderAuth::Missing => Some("Not connected".to_string()),
            ProviderAuth::Checking => Some("Checking key…".to_string()),
            ProviderAuth::Problem(problem) => Some(without_name(problem, &p.name)),
        };
        let access = match p.group { ProviderGroup::OnThisMac => Access::ThisMac, ProviderGroup::Subscription => Access::Subscription, ProviderGroup::ApiKey => Access::Api };
        Some(Provider { id: p.id.clone(), title: p.title(), via: (access == Access::ThisMac).then(|| Access::ThisMac.label().to_string()),
            icon: p.icon.clone(), access, reason, models })
    };
    let group = |g: ProviderGroup| api.providers.iter().filter(move |p| p.group == g);
    out.extend(group(ProviderGroup::OnThisMac).filter_map(api_row));
    for i in t3 {
        let (title, icon, via) = t3_title(i);
        let models = i.models.iter().map(|m| Model { id: m.id.clone(), name: m.name.clone(), levels: m.efforts.clone(), default_level: m.default_effort.clone(), reason: None }).collect();
        out.push(Provider { id: format!("{T3_PROVIDER_PREFIX}{}", i.instance), title, via: Some(via), icon, access: Access::Subscription, reason: None, models });
    }
    // A T3 choice whose instance T3 has not listed (T3 Code closed, or its list not here yet): kept, and why.
    if let Some(instance) = t3_instance(&selected.0).filter(|i| !t3.iter().any(|t| t.instance == *i)) {
        let known = T3Instance { instance: instance.into(), name: if instance == T3_DEFAULT_INSTANCE { "Claude code 2".into() } else { instance.into() }, driver: "claudeAgent".into(), models: vec![] };
        let (title, icon, via) = t3_title(&known);
        out.push(Provider { id: chosen.0.clone(), title, via: Some(via), icon, access: Access::Subscription, reason: Some("T3 Code not running".into()),
            models: vec![Model { id: chosen.1.clone(), name: chosen.1.clone(), levels: vec![], default_level: None, reason: None }] });
    }
    out.extend(group(ProviderGroup::Subscription).filter_map(api_row));
    out.extend(group(ProviderGroup::ApiKey).filter_map(api_row));
    // The app's hub listed no Anthropic yet: its Haiku still shows, waiting for a key.
    if !out.iter().any(|p| p.id == BACKGROUND_DEFAULT.0) {
        out.push(Provider { id: BACKGROUND_DEFAULT.0.into(), title: "Anthropic API".into(), via: None, icon: "anthropic".into(), access: Access::Api,
            reason: Some("Not connected".into()), models: vec![Model { id: BACKGROUND_DEFAULT.1.into(), name: "Claude Haiku 5.5".into(), levels: vec![], default_level: None, reason: None }] });
    }
    out
}

/// "Low", "XHigh", "Ultrathink": a level as the selector and the caption show it.
pub(crate) fn level_title(level: &str) -> String {
    let t = thinking_title(level);
    if level == "xhigh" { return "XHigh".into(); }
    if t != level { return t; }
    let mut c = level.chars();
    c.next().map_or_else(String::new, |f| f.to_uppercase().chain(c).collect())
}

/// The level the user chose for a model in Background Tasks (`Settings::background_efforts`).
pub(crate) fn chosen_level<'a>(settings: &'a Settings, provider: &str, id: &str) -> Option<&'a str> {
    let provider = normal(provider);
    settings.background_efforts.iter().find(|(p, i, _)| normal(p) == provider && i == id).map(|(_, _, l)| l.as_str())
}

/// Whether a row is the choice (`t3:` alone and the default instance are one; the engine's row is whatever it serves).
fn row_on(selected: &(String, String), provider: &str, id: &str) -> bool {
    menu::background_row_on(&(normal(&selected.0), selected.1.clone()), provider, id)
}

/// The section's caption: the model, where it runs and, once chosen, its level: "Claude Haiku 5.5 · Claude code 2 ·
/// Low", "Claude Haiku 5.5 · Anthropic". An API or local choice is named as `menu::background_caption` names it.
pub(crate) fn caption(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings) -> String {
    caption_of(api, engine, t3, settings, &settings.background_model)
}

/// A choice's caption (`caption`): model, where it runs, the level once chosen.
fn caption_of(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings, selected: &(String, String)) -> String {
    let base = match t3_instance(&selected.0) {
        Some(instance) => {
            let i = t3.iter().find(|i| i.instance == instance);
            let name = i.and_then(|i| i.models.iter().find(|m| m.id == selected.1)).map_or_else(|| selected.1.clone(), |m| m.name.clone());
            let place = i.map_or_else(|| if instance == T3_DEFAULT_INSTANCE { "Claude code 2".to_string() } else { "Claude Code subscription".to_string() }, |i| i.name.clone());
            format!("{name} · {place}")
        }
        None => menu::background_caption(api, engine, selected),
    };
    match chosen_level(settings, &selected.0, &selected.1) {
        Some(level) if !selected.0.is_empty() => format!("{base} · {}", level_title(level)),
        _ => base,
    }
}

/// Which choice a picker sets: the shared Background Tasks model, or one task's own (the island
/// agent on Claude Haiku 5.5 through T3, the other jobs on the shared model).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Scope {
    #[default]
    Shared,
    Task(BackgroundTask),
}

/// A task's own model (`Settings::task_models`), if it has one.
pub(crate) fn task_choice(settings: &Settings, task: BackgroundTask) -> Option<(String, String)> {
    settings.task_models.iter().find(|(t, p, _)| *t == task && !p.is_empty()).map(|(_, p, i)| (normal(p), i.clone()))
}

impl Scope {
    /// The radios' choice: the shared model; a task's own, or none (its "Use Default Model" row).
    fn chosen(self, settings: &Settings) -> Option<(String, String)> {
        match self { Scope::Shared => Some(settings.background_model.clone()), Scope::Task(t) => task_choice(settings, t) }
    }
    /// Whether its picker offers T3's instances: Suggestions is a tool loop, which a T3 thread cannot run (
    /// 07:37).
    pub(crate) fn t3(self) -> bool {
        self != Scope::Task(BackgroundTask::Suggestions)
    }
    /// The picker's content for this scope: T3's instances only where they can run, and no stand-in row for a T3
    /// choice they cannot.
    fn shape(self, api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings) -> Vec<Provider> {
        let runs = self.runs(settings);
        if self.t3() { return providers(api, engine, t3, &runs); }
        providers(api, engine, &[], &if t3_instance(&runs.0).is_some() { Default::default() } else { runs })
    }
    /// The model the scope runs: a task without its own runs the shared one.
    fn runs(self, settings: &Settings) -> (String, String) {
        self.chosen(settings).unwrap_or_else(|| settings.background_model.clone())
    }
}

/// A task's own model row's title: "Use Default Model", first in its picker.
pub(crate) const SAME_AS_SHARED: &str = "Use Default Model";

/// One provider row's live parts: its disclosure and the model rows (indices into `Rows::models`) it shows.
pub(crate) struct ProviderRow {
    pub id: String,
    pub item: Retained<NSMenuItem>,
    pub disclosure: Retained<NSButton>,
    pub models: std::ops::Range<usize>,
}

/// One model's thinking row: the item, its selector (none for a single level) and the levels by segment.
pub(crate) struct ThinkingRow {
    pub item: Retained<NSMenuItem>,
    pub control: Option<Retained<NSSegmentedControl>>,
    pub levels: Vec<String>,
}

/// A picker's live rows. A radio's or selector's tag indexes `models`; a disclosure's, `providers`. A task's picker
/// starts with its Use Default Model radio (provider and id empty, outside every provider).
#[derive(Default)]
pub(crate) struct Rows {
    pub scope: Scope,
    /// (radio, provider, model id) per model row.
    pub models: Vec<(Retained<NSButton>, String, String)>,
    /// Each model row's thinking row, by the same index (`None`: the model has no level to pick).
    pub thinking: Vec<Option<ThinkingRow>>,
    pub providers: Vec<ProviderRow>,
    /// The user's open or closed choice per provider this run (else: open while it holds the choice).
    pub open: BTreeMap<String, bool>,
    /// What the rows were drawn from: a change lays them again.
    pub shape: Vec<Provider>,
}

/// A task's own Model row in Background Tasks (under its toggle) and the picker it opens.
pub(crate) struct TaskPicker {
    pub task: BackgroundTask,
    pub item: Retained<NSMenuItem>,
    pub menu: Retained<NSMenu>,
    pub rows: Rows,
}

/// The tasks with a model of their own: every one ( the island agent, 07:37 all of them).
pub(crate) const OWN_MODEL_TASKS: [BackgroundTask; 6] = BackgroundTask::ALL;

/// Builds the picker for the menu: the rows go into `state`, the caption follows.
pub(crate) fn menu(state: &mut State, target: Option<&AnyObject>, mtm: MainThreadMarker) -> Retained<NSMenu> {
    let open = std::mem::take(&mut state.background_rows.open);
    let (sub, mut rows) = build(&state.api, &state.engine, &state.t3_instances, &state.settings_value, &open, Scope::Shared, target, mtm);
    rows.open = open;
    state.background_rows = rows;
    sync_caption(state);
    sub
}

/// One row per task in the status menu (climb 2, Background Tasks "exploded in size"): its symbol
/// and name, what it runs as the grey value when that is not the shared model, "Off" when off. Its submenu holds all of
/// the task: its switch first (the menu's `CheckRow`, kept in `background_task_rows`), then its own picker (
/// 07:37). The rows go into `into` at the end.
pub(crate) fn add_task_rows(state: &mut State, into: &NSMenu, target: Option<&AnyObject>, mtm: MainThreadMarker) {
    let mut old: Vec<TaskPicker> = std::mem::take(&mut state.task_pickers);
    for task in OWN_MODEL_TASKS {
        let open = old.iter_mut().find(|p| p.task == task).map(|p| std::mem::take(&mut p.rows.open)).unwrap_or_default();
        let (sub, mut rows) = build(&state.api, &state.engine, &state.t3_instances, &state.settings_value, &open, Scope::Task(task), target, mtm);
        rows.open = open;
        let item = menu_item(task.title(), Some(task.icon()), None, "", None, mtm);
        item.setSubmenu(Some(&sub));
        into.addItem(&item);
        state.task_pickers.push(TaskPicker { task, item, menu: sub, rows });
    }
    for n in 0..state.task_pickers.len() { prepend_toggle(state, n); }
    show_menu_images(into);
    sync_task_rows(state);
}

/// The task's switch and a line at the top of its submenu; again after the picker is laid anew (`relay`).
fn prepend_toggle(state: &State, n: usize) {
    let p = &state.task_pickers[n];
    let Some((_, toggle)) = state.background_task_rows.iter().find(|(t, _)| *t == p.task) else { return };
    // The switch's own item, its action and tag kept (Return on the highlighted row runs it).
    let item = toggle.enclosingMenuItem().unwrap_or_else(|| {
        let target = state.target();
        let item = menu_item(p.task.title(), Some(p.task.icon()), Some(sel!(toggleBackgroundTask:)), "", Some(&*target), state.mtm);
        item.setTag(menu::background_task_row(p.task) as isize);
        item.setView(Some(&**toggle));
        item
    });
    if let Some(menu) = unsafe { item.menu() } { menu.removeItem(&item); }
    p.menu.insertItem_atIndex(&item, 0);
    p.menu.insertItem_atIndex(&NSMenuItem::separatorItem(state.mtm), 1);
}

/// The picker's menu and rows from its inputs (the menu and the offscreen renders build it the same way); `open`
/// holds the user's open/closed choices; `scope` which choice its radios set.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings, open: &BTreeMap<String, bool>,
    scope: Scope, target: Option<&AnyObject>, mtm: MainThreadMarker) -> (Retained<NSMenu>, Rows) {
    let sub = NSMenu::new(mtm);
    sub.setAutoenablesItems(false);
    let chosen = scope.chosen(settings);
    let selected = chosen.clone().unwrap_or_default();
    let shape = scope.shape(api, engine, t3, settings);
    let font = &*menu_font();
    let reason_width = |r: &Option<String>| r.as_ref().map_or(0.0, |u| text_width(&format!(" — {u}"), font));
    // Native width: the providers and model rows decide it; a level selector, shown under the chosen model, may widen
    // it, as an opened model's details widen Agent ▸ Model.
    let width = shape.iter().map(|p| PROVIDER_X + text_width(&p.title, font) + p.via.as_ref().map_or(0.0, |v| text_width(&format!(" · {v}"), font))
            + reason_width(&p.reason) + DISCLOSURE_INSET + 16.0)
        .chain(shape.iter().flat_map(|p| p.models.iter().map(move |m| MODEL_TITLE_X + text_width(&m.name, font) + reason_width(&m.reason) + 24.0)))
        .chain(matches!(scope, Scope::Task(_)).then(|| MODEL_X + 20.0 + text_width(&format!("{SAME_AS_SHARED}  {}", quiet(&caption(api, engine, t3, settings))), font) + 24.0))
        .fold(menu::MIN_SUBMENU, f64::max).ceil();
    let choose = match scope { Scope::Shared => sel!(chooseBackgroundModel:), Scope::Task(_) => sel!(chooseTaskModel:) };
    let mut rows = Rows { scope, ..Default::default() };
    // A task's picker: first, back to the shared model.
    if matches!(scope, Scope::Task(_)) {
        let (line, radio) = menu::control_row(SAME_AS_SHARED, choose, chosen.is_none(), false, width, 0, target, mtm);
        radio.setFrame(rect(12.0, 2.0, width - 24.0, 24.0));
        sub.addItem(&line);
        rows.models.push((radio, String::new(), String::new()));
        rows.thinking.push(None);
        sub.addItem(&NSMenuItem::separatorItem(mtm));
    }
    let mac = shape.iter().any(|p| p.access == Access::ThisMac);
    let mut cloud_header = false;
    if mac { sub.addItem(&NSMenuItem::sectionHeaderWithTitle(&ns(menu::ON_THIS_MAC), mtm)); }
    for p in &shape {
        if p.access != Access::ThisMac && !cloud_header {
            if mac { sub.addItem(&NSMenuItem::separatorItem(mtm)); }
            sub.addItem(&NSMenuItem::sectionHeaderWithTitle(&ns(menu::CLOUD), mtm));
            cloud_header = true;
        }
        let is_open = open.get(&p.id).copied().unwrap_or(chosen.as_ref().is_some_and(|c| normal(&c.0) == p.id));
        let (item, disclosure) = provider_item(p, is_open, rows.providers.len() as isize, width, target, mtm);
        sub.addItem(&item);
        let from = rows.models.len();
        for m in &p.models {
            let tag = rows.models.len() as isize;
            let on = row_on(&selected, &p.id, &m.id);
            let (line, radio) = menu::control_row(&m.name, choose, on, false, width, tag, target, mtm);
            radio.setFrame(rect(MODEL_X, 2.0, width - MODEL_X - 12.0, 24.0));
            let reason = if p.reason.is_some() { None } else { m.reason.as_deref() };
            radio.setAttributedTitle(&menu::model_title(&m.name, None, reason));
            // The chosen row stays enabled even when it cannot run: it is the choice, and its provider says why.
            radio.setEnabled(on || (p.reason.is_none() && m.reason.is_none()));
            line.setHidden(!is_open);
            sub.addItem(&line);
            rows.models.push((radio, p.id.clone(), m.id.clone()));
            let thinking = thinking_row(m, settings, &p.id, width, tag, target, mtm);
            if let Some(t) = &thinking {
                t.item.setHidden(!(is_open && on));
                sub.addItem(&t.item);
            }
            rows.thinking.push(thinking);
        }
        rows.providers.push(ProviderRow { id: p.id.clone(), item, disclosure, models: from..rows.models.len() });
    }
    sub.addItem(&NSMenuItem::separatorItem(mtm));
    // [providers] No Add Provider here (all provider work lives in Agent ▸ Model): this row opens Agent ▸ Model under
    // the menu bar item.
    sub.addItem(&menu_item(menu::BACKGROUND_ADD_MODEL, Some("plus.circle"), Some(sel!(openAgentModels:)), "", target, mtm));
    menu::hide_group_headers(&sub);
    show_menu_images(&sub);
    rows.shape = shape;
    (sub, rows)
}

/// A provider's row: its mark and title (the instance, why it cannot run, in grey), and Agent ▸ Model's disclosure
/// at the right; a press anywhere on the row opens or closes it in place (`ControlRow` clicks its one button).
fn provider_item(p: &Provider, open: bool, tag: isize, width: f64, target: Option<&AnyObject>, mtm: MainThreadMarker) -> (Retained<NSMenuItem>, Retained<NSButton>) {
    let item = menu_item(&p.title, None, None, "", None, mtm);
    let host = controls::ControlRow::new(rect(0.0, 0.0, width, 28.0), mtm);
    // The mark in the menu's icon column, tinted like a native item's image (a template follows the appearance).
    if let Some(image) = mark(&p.icon) {
        let view = objc2_app_kit::NSImageView::imageViewWithImage(&image, mtm);
        view.setContentTintColor(Some(&label()));
        view.setFrame(rect((ICON_X - 8.0).round(), 6.0, 16.0, 16.0));
        host.addSubview(&view);
    }
    let title = objc2_app_kit::NSTextField::labelWithAttributedString(&menu::model_title_via(&p.title, None, p.via.as_deref(), p.reason.as_deref()), mtm);
    title.setLineBreakMode(objc2_app_kit::NSLineBreakMode::ByTruncatingTail);
    let height = title.intrinsicContentSize().height.ceil();
    // A label's text sits 2 pt inside its frame: the title starts in the native rows' title column.
    title.setFrame(rect(PROVIDER_X - 2.0, ((28.0 - height) / 2.0).round(), width - PROVIDER_X - DISCLOSURE_INSET - 6.0, height));
    title.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable);
    host.addSubview(&title);
    let disclosure = unsafe { NSButton::buttonWithTitle_target_action(&ns(""), target, Some(sel!(toggleBackgroundProvider:)), mtm) };
    disclosure.setBezelStyle(objc2_app_kit::NSBezelStyle::Disclosure);
    disclosure.setButtonType(objc2_app_kit::NSButtonType::PushOnPushOff);
    disclosure.setTitle(&ns(""));
    disclosure.setState(if open { NSControlStateValueOn } else { NSControlStateValueOff });
    disclosure.setTag(tag);
    disclosure.setFrame(rect(width - 26.0, 4.0, 20.0, 20.0));
    disclosure.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin);
    disclosure.setToolTip(Some(&ns("Models")));
    host.addSubview(&disclosure);
    item.setView(Some(&host));
    (item, disclosure)
}

/// A provider's mark: its template image (`menu::provider_image`), else the symbol Agent ▸ Model draws for it.
fn mark(icon: &str) -> Option<Retained<objc2_app_kit::NSImage>> {
    if let Some(image) = menu::provider_image(icon) { return Some(image); }
    let name = match icon { "local" => "laptopcomputer", "custom" => "server.rack", "openrouter" => "arrow.triangle.branch", _ => "network" };
    let image = symbol_sized(name, 13.0, unsafe { objc2_app_kit::NSFontWeightRegular })?;
    image.setTemplate(true);
    Some(image)
}

/// A model's thinking row, under its radio's title: Agent ▸ Model's selector of its levels (`menu::thinking_item`), a
/// grey "Thinking Max" for a single level, nothing without levels. Selected: the user's level, else T3's default.
#[allow(clippy::too_many_arguments)]
fn thinking_row(m: &Model, settings: &Settings, provider: &str, width: f64, tag: isize, target: Option<&AnyObject>, mtm: MainThreadMarker) -> Option<ThinkingRow> {
    if m.levels.is_empty() { return None; }
    let current = chosen_level(settings, provider, &m.id).map(str::to_owned).or_else(|| m.default_level.clone()).unwrap_or_default();
    if m.levels.len() == 1 {
        let (item, row) = menu::label_item(MODEL_TITLE_X, mtm);
        row.set(&format!("{THINKING} {}", level_title(&m.levels[0])), false, false);
        return Some(ThinkingRow { item, control: None, levels: m.levels.clone() });
    }
    // A model with no default of its own gets a first "Default" segment: no level chosen, each job runs its own effort
    // (the app's empty level). It shows selected until a level is picked (climb 2: an unselected control read as broken,
    // and lighting the agent's level was false).
    let levels: Vec<String> = if m.default_level.is_none() { std::iter::once(String::new()).chain(m.levels.iter().cloned()).collect() } else { m.levels.clone() };
    let titles: Vec<String> = levels.iter().map(|l| if l.is_empty() { DEFAULT_LEVEL.to_string() } else { level_title(l) }).collect();
    let labels = text_width(THINKING, &menu_font()).ceil() + 8.0;
    let current_title = if current.is_empty() { DEFAULT_LEVEL.to_string() } else { level_title(&current) };
    let (item, control) = menu::thinking_item(&titles, &current_title, width, labels, sel!(backgroundEffort:), tag, target, mtm);
    if let Some(c) = &control { c.setSelectedSegment(levels.iter().position(|l| *l == current).map_or(-1, |i| i as isize)); }
    // Agent ▸ Model lays it under a top-level radio's title; here the radios sit one step in.
    let shift = MODEL_TITLE_X - MODEL_X;
    if let Some(host) = item.view() {
        for v in host.subviews().iter() {
            let mut f = v.frame();
            f.origin.x += shift;
            v.setFrame(f);
        }
        let right = control.as_ref().map_or(0.0, |c| c.frame().origin.x + c.frame().size.width) + 16.0;
        host.setFrameSize(objc2_foundation::NSSize::new(width.max(right).ceil(), host.frame().size.height));
    }
    Some(ThinkingRow { item, control, levels })
}

/// The thinking selector's first segment while no level is chosen for the jobs: each runs its own effort.
pub(crate) const DEFAULT_LEVEL: &str = "Default";

/// While the menu tracks: new content lays a picker's rows again in its own submenu (a T3 instance or a model
/// arrived); otherwise the radios, the selectors and which thinking row shows follow the choice, and the captions
/// follow. The shared picker, then each task's.
pub(crate) fn sync(state: &mut State) {
    let shape = providers(&state.api, &state.engine, &state.t3_instances, &state.settings_value.background_model);
    if shape != state.background_rows.shape {
        if let Some(sub) = state.background_menu.clone() {
            // What is open stays open.
            let rows = &mut state.background_rows;
            for p in &rows.providers { rows.open.insert(p.id.clone(), p.disclosure.state() == NSControlStateValueOn); }
            let (target, mtm) = (state.target(), state.mtm);
            let fresh = menu(state, Some(&*target), mtm);
            relay(&fresh, &sub);
        }
    } else {
        sync_rows(&state.background_rows, &state.settings_value);
    }
    for n in 0..state.task_pickers.len() {
        let scope = state.task_pickers[n].rows.scope;
        let shape = scope.shape(&state.api, &state.engine, &state.t3_instances, &state.settings_value);
        if shape == state.task_pickers[n].rows.shape {
            sync_rows(&state.task_pickers[n].rows, &state.settings_value);
            continue;
        }
        let rows = &mut state.task_pickers[n].rows;
        for p in &rows.providers { rows.open.insert(p.id.clone(), p.disclosure.state() == NSControlStateValueOn); }
        let open = std::mem::take(&mut rows.open);
        let target = state.target();
        let (fresh, mut rows) = build(&state.api, &state.engine, &state.t3_instances, &state.settings_value, &open, scope, Some(&*target), state.mtm);
        rows.open = open;
        relay(&fresh, &state.task_pickers[n].menu);
        state.task_pickers[n].rows = rows;
        prepend_toggle(state, n);
    }
    sync_caption(state);
    sync_task_rows(state);
}

/// `fresh`'s items moved into the open `into`, in order: the submenu on screen stays the same object.
fn relay(fresh: &NSMenu, into: &NSMenu) {
    into.removeAllItems();
    for item in fresh.itemArray().iter() {
        fresh.removeItem(&item);
        into.addItem(&item);
    }
    show_menu_images(into);
}

/// One picker's radios, selectors and thinking rows follow the choice, in place.
fn sync_rows(rows: &Rows, settings: &Settings) {
    let chosen = rows.scope.chosen(settings);
    let selected = chosen.clone().unwrap_or_default();
    if let Some((radio, ..)) = rows.models.first().filter(|(_, p, _)| p.is_empty()) {
        radio.setState(if chosen.is_none() { NSControlStateValueOn } else { NSControlStateValueOff });
    }
    for p in &rows.providers {
        let open = p.disclosure.state() == NSControlStateValueOn;
        for i in p.models.clone() {
            let (radio, provider, id) = &rows.models[i];
            let on = row_on(&selected, provider, id);
            radio.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
            let usable = rows.shape.iter().find(|s| &s.id == provider).is_some_and(|s| s.reason.is_none() && s.models.iter().any(|m| &m.id == id && m.reason.is_none()));
            radio.setEnabled(on || usable);
            if let Some(Some(t)) = rows.thinking.get(i) {
                t.item.setHidden(!(open && on));
                let level = chosen_level(settings, provider, id).map(str::to_owned)
                    .or_else(|| rows.shape.iter().find(|s| &s.id == provider).and_then(|s| s.models.iter().find(|m| &m.id == id)).and_then(|m| m.default_level.clone()))
                    .unwrap_or_default();
                if let Some(c) = &t.control {
                    if let Some(at) = t.levels.iter().position(|x| *x == level) { c.setSelectedSegment(at as isize); }
                }
            }
        }
    }
}

/// The Default Model row: one line, its model as a quiet short name on the right ("Luna"; "too
/// much text"); where it runs, its level and the tasks it serves are its tooltip. The old caption line stays hidden.
fn sync_caption(state: &State) {
    let line = caption(&state.api, &state.engine, &state.t3_instances, &state.settings_value);
    if let Some(row) = &state.background_caption {
        row.set(&menu::one_line(&line), false, false);
        if let Some(item) = row.enclosingMenuItem() { item.setHidden(true); }
    }
    let Some(sub) = &state.background_menu else { return };
    let Some(item) = state.menu.itemArray().iter().find(|i| i.submenu().is_some_and(|m| std::ptr::eq(&*m, &**sub))) else { return };
    // What it serves now: not the tasks a fallback moved off it, none while emergency mode pauses every task.
    let inheriting: Vec<BackgroundTask> = if state.relay.emergency() { vec![] } else {
        inheriting(&state.settings_value).into_iter().filter(|t| !state.background_routes.iter().any(|(r, ..)| r == t)).collect()
    };
    let (value, mark) = marked_value(&line, menu::BACKGROUND_MODEL_TITLE, mark_of(&state.api, &state.engine, &state.t3_instances, &state.settings_value, &state.settings_value.background_model));
    menu::set_value_marked(&item, Some(&value), mark.as_deref());
    menu::set_subtitle(&item, None);
    let names: Vec<&str> = inheriting.iter().map(|t| t.title()).collect();
    let tip = match names.as_slice() {
        [] => format!("{line}. No task runs it now"),
        [one] => format!("{line}. The default for {one}, which has no model of its own"),
        [rest @ .., last] => format!("{line}. The default for {} and {last}, which have no model of their own", rest.join(", ")),
    };
    item.setToolTip(Some(&ns(&tip)));
}

/// A caption's model as a row's quiet value: its name alone, short ("GPT-6 Luna · Codex subscription" → "Luna"), cut
/// at one width for every row of the section.
pub(crate) fn quiet(line: &str) -> String {
    quiet_after(line, "")
}

/// `quiet` after `prefix` (a fallback's "↪ "), the prefix's room coming off the name's: cut at one width in every task
/// row (the Default Model's longer title measures its own row, `marked_value`).
fn quiet_after(line: &str, prefix: &str) -> String {
    let titles: Vec<String> = OWN_MODEL_TASKS.iter().map(|t| format!("{}{prefix}", t.title())).collect();
    let titles: Vec<&str> = titles.iter().map(String::as_str).collect();
    let name = menu::quiet_name(line.split(" \u{b7} ").next().unwrap_or_default());
    format!("{prefix}{}", menu::fit_value_beside(&titles, &name))
}

/// A row that chooses a model (`title`): its short name after the provider's mark, cut to the room left beside both.
fn marked_value(line: &str, title: &str, mark: Option<String>) -> (String, Option<String>) {
    let name = menu::quiet_name(line.split(" \u{b7} ").next().unwrap_or_default());
    // The mark stays (round 3: dropping it on a long name lost the row's sign); the name gives way around it.
    match mark {
        Some(mark) => (menu::fit_value_beside(&[&format!("{title}\u{2003}\u{2002}")], &name), Some(mark)),
        None => (menu::fit_value_beside(&[title], &name), None),
    }
}

/// The provider mark (`menu::provider_image`'s name) of a choice: Anthropic's for Claude through T3.
/// A T3 instance's is its driver's (`t3_title`), as the picker's own row shows it; a provider no longer listed still gets
/// a generic mark, so a chosen model never reads as an inherited one (code review).
fn mark_of(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings, selected: &(String, String)) -> Option<String> {
    if selected.0.is_empty() { return None }
    if let Some(instance) = t3_instance(&selected.0) {
        if let Some(i) = t3.iter().find(|i| i.instance == instance) { return Some(t3_title(i).1) }
        return Some(if instance == T3_DEFAULT_INSTANCE || !instance.to_lowercase().contains("codex") { "anthropic" } else { "codex" }.to_string());
    }
    let shape = Scope::Shared.shape(api, engine, t3, settings);
    Some(shape.into_iter().find(|p| p.id == normal(&selected.0)).map_or_else(|| "network".to_string(), |p| p.icon))
}

/// Background Tasks' row values set again (after `menu::settle_width` moves the values' stop).
pub(crate) fn sync_values(state: &State) {
    sync_task_rows(state);
}

/// A task's model as the paused stand-ins show it (emergency mode): its short name, nothing while it is off.
pub(crate) fn paused_value(state: &State, task: BackgroundTask) -> Option<String> {
    (!state.settings_value.background_off.contains(&task)).then(|| quiet(&task_caption(&state.api, &state.engine, &state.t3_instances, &state.settings_value, task)))
}

/// The tasks that run the Default Model: on, and without a model of their own.
pub(crate) fn inheriting(settings: &Settings) -> Vec<BackgroundTask> {
    OWN_MODEL_TASKS.into_iter().filter(|t| !settings.background_off.contains(t) && task_choice(settings, *t).is_none()).collect()
}

/// A task's own Model row: the model it runs ("Claude Haiku 5.5 · Claude code 2"), a native row in the title
/// column under the task's toggle, its chevron to the picker; hidden while the task is off.
pub(crate) fn task_caption(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings, task: BackgroundTask) -> String {
    caption_of(api, engine, t3, settings, &Scope::Task(task).runs(settings))
}

fn sync_task_rows(state: &State) {
    sync_caption(state);
    // Each task picker's Use Default Model row names the default after it, in grey (climb round 3: which model it was
    // meant opening another submenu).
    let default = quiet(&caption(&state.api, &state.engine, &state.t3_instances, &state.settings_value));
    for p in &state.task_pickers {
        let Some((radio, ..)) = p.rows.models.first() else { continue };
        let text = objc2_foundation::NSMutableAttributedString::new();
        text.appendAttributedString(&super::util::attributed(SAME_AS_SHARED, &menu_font(), &super::util::label()));
        text.appendAttributedString(&super::util::attributed(&format!("  {default}"), &menu_font(), &super::util::secondary()));
        let wanted = if state.settings_value.background_model.0.is_empty() { SAME_AS_SHARED.to_string() } else { format!("{SAME_AS_SHARED}  {default}") };
        if state.settings_value.background_model.0.is_empty() { text.mutableString().setString(&ns(SAME_AS_SHARED)); }
        if radio.attributedTitle().string().to_string() != wanted { radio.setAttributedTitle(&text); }
    }
    let emergency = state.relay.emergency();
    for p in &state.task_pickers {
        let off = state.settings_value.background_off.contains(&p.task);
        // A task that is off answers on nothing, whatever route the app last reported for it.
        let route = state.background_routes.iter().find(|(t, ..)| *t == p.task && !off).map(|(_, provider, id)| (provider.clone(), id.clone()));
        let own = Scope::Task(p.task).chosen(&state.settings_value).is_some();
        let caption = task_caption(&state.api, &state.engine, &state.t3_instances, &state.settings_value, p.task);
        // One line: the row's value is the model it really runs, as a quiet short name: off; on a
        // fallback, the arrow and what answers now; its own model after its provider's mark; the Default Model's
        // plain, so a model of its own stands out without a "Default" word. Where it runs is the tooltip.
        let (value, mark) = if off { (Some(OFF.to_string()), None) } else {
            match &route {
                // What answers now, by its model's name like every other row (both judges, climb 3 Nov round 0: "↪ Claude"
                // named a place, not a model); the orange arrow says it changed, the tooltip where it runs and why.
                Some(now) => {
                    let line = caption_of(&state.api, &state.engine, &state.t3_instances, &state.settings_value, now);
                    (Some(quiet_after(&line, "\u{21aa} ")), None)
                }
                None if own => {
                    let (value, mark) = marked_value(&caption, p.task.title(), mark_of(&state.api, &state.engine, &state.t3_instances, &state.settings_value, &Scope::Task(p.task).runs(&state.settings_value)));
                    (Some(value), mark)
                }
                None => (Some(quiet(&caption)), None),
            }
        };
        menu::set_subtitle(&p.item, None);
        // Off: "Off" as the value; the title keeps its ink, the row being the way to switch it on again.
        menu::set_value_marked(&p.item, value.as_deref(), mark.as_deref());
        let tip = match route {
            Some(r) => fallback_tip(&state.api, &state.engine, &state.t3_instances, &state.settings_value, p.task, &r),
            None if off => format!("{} is off", p.task.title()),
            None if own => caption.clone(),
            None if state.settings_value.background_model.0.is_empty() => "No default model chosen".to_string(),
            None => format!("{caption}, the default model"),
        };
        p.item.setToolTip(Some(&ns(&tip)));
        // Emergency mode: the greyed stand-ins take the rows' place.
        p.item.setHidden(emergency);
    }
}

/// A task row's value while the task is off.
pub(crate) const OFF: &str = "Off";

/// The sign's tooltip: the pick it stands in for, why that cannot run when the picker knows (its provider's
/// reason), and what runs instead.
fn fallback_tip(api: &ApiProviders, engine: &EngineStatus, t3: &[T3Instance], settings: &Settings, task: BackgroundTask, now: &(String, String)) -> String {
    let scope = Scope::Task(task);
    let pick = scope.runs(settings);
    let why = scope.shape(api, engine, t3, settings).into_iter().find(|p| p.id == normal(&pick.0)).and_then(|p| p.reason)
        .or_else(|| (!scope.t3() && t3_instance(&pick.0).is_some()).then(|| "T3 cannot run its tools".to_string()));
    let pick = caption_of(api, engine, t3, settings, &pick);
    let why = why.map_or_else(String::new, |w| format!(" ({w})"));
    format!("{} runs on {} while {pick} cannot{why}", task.title(), caption_of(api, engine, t3, settings, now))
}

/// The picker holding `found` (the shared one: `None`), and the index `found` gave.
fn locate(state: &State, found: impl Fn(&Rows) -> Option<usize>) -> Option<(Option<usize>, usize)> {
    if let Some(i) = found(&state.background_rows) { return Some((None, i)); }
    state.task_pickers.iter().enumerate().find_map(|(n, p)| found(&p.rows).map(|i| (Some(n), i)))
}

fn rows_mut(state: &mut State, which: Option<usize>) -> &mut Rows {
    match which { None => &mut state.background_rows, Some(n) => &mut state.task_pickers[n].rows }
}

fn same<T: objc2::Message>(a: &T, b: &T) -> bool { std::ptr::eq(a, b) }

/// A provider's disclosure (`sender`): its models (and the chosen one's thinking row) show or hide in place; the
/// choice holds for this run.
pub(crate) fn toggle(state: &mut State, sender: &NSButton, open: bool) {
    let Some((which, index)) = locate(state, |r| r.providers.iter().position(|p| same(&*p.disclosure, sender))) else { return };
    let settings = state.settings_value.clone();
    let rows = rows_mut(state, which);
    let id = rows.providers[index].id.clone();
    rows.open.insert(id, open);
    let p = &rows.providers[index];
    if p.disclosure.state() != if open { NSControlStateValueOn } else { NSControlStateValueOff } {
        p.disclosure.setState(if open { NSControlStateValueOn } else { NSControlStateValueOff });
    }
    let selected = rows.scope.chosen(&settings).unwrap_or_default();
    for i in p.models.clone() {
        let (radio, provider, id) = &rows.models[i];
        if let Some(item) = radio.enclosingMenuItem() { item.setHidden(!open); }
        if let Some(Some(t)) = rows.thinking.get(i) { t.item.setHidden(!(open && row_on(&selected, provider, id))); }
    }
}

/// A model's level picked on its selector (`sender`): kept and sent to the app (one level per model, whichever
/// picker shows it); the captions name it.
pub(crate) fn level_picked(state: &mut State, sender: &NSSegmentedControl) {
    let found = locate(state, |r| r.thinking.iter().position(|t| t.as_ref().and_then(|t| t.control.as_ref()).is_some_and(|c| same(&**c, sender))));
    let Some((which, index)) = found else { return };
    let rows = rows_mut(state, which);
    let Some(Some(t)) = rows.thinking.get(index) else { return };
    let Some(level) = usize::try_from(sender.selectedSegment()).ok().and_then(|i| t.levels.get(i)).cloned() else { return };
    let Some((_, provider, id)) = rows.models.get(index).cloned() else { return };
    if chosen_level(&state.settings_value, &provider, &id).unwrap_or_default() == level { return; }
    let efforts = &mut state.settings_value.background_efforts;
    efforts.retain(|(p, i, _)| !(normal(p) == provider && *i == id));
    // Default (the empty level): no level kept, each job's own effort again.
    if !level.is_empty() { efforts.push((provider.clone(), id.clone(), level.clone())); }
    state.emit(UiAction::SetBackgroundEffort { provider, id, level });
    sync_caption(state);
    sync_task_rows(state);
}

/// A task's picker ▸ a model (`sender`, its radio): the task's own model from now on, or back to the shared one. A
/// row that cannot run is refused; the choice it already has sends nothing.
pub(crate) fn task_chosen(state: &mut State, sender: &NSButton) {
    let found = locate(state, |r| r.models.iter().position(|(b, ..)| same(&**b, sender)));
    if let Some((Some(n), index)) = found {
        let task = state.task_pickers[n].task;
        let (radio, provider, id) = state.task_pickers[n].rows.models[index].clone();
        let current = task_choice(&state.settings_value, task);
        let wanted = (!provider.is_empty()).then(|| (provider.clone(), id.clone()));
        if radio.isEnabled() && current != wanted {
            state.settings_value.task_models.retain(|(t, ..)| *t != task);
            if wanted.is_some() { state.settings_value.task_models.push((task, provider.clone(), id.clone())); }
            state.emit(UiAction::SetTaskModel { task, provider, id });
        }
    }
    state.refresh_menu();
}

/// [menu lane] `heimdall-ui-demo --background-picker`'s data, matching T3's live `orchestrator_capabilities` of 30 Oct:
/// Claude code 2 (Opus, Sonnet, Fable, Haiku 5.5, efforts low … ultrathink, Haiku's default medium) and Codex
/// (GPT-6 Luna, low … max).
#[doc(hidden)]
pub(crate) fn demo_t3() -> Vec<T3Instance> {
    let efforts = |extra: bool| { let mut e = vec!["low", "medium", "high", "xhigh", "max"]; if extra { e.push("ultracode"); } e.push("ultrathink"); e.into_iter().map(String::from).collect::<Vec<_>>() };
    let claude = |id: &str, name: &str, extra: bool, default: &str| T3Model { id: id.into(), name: name.into(), efforts: efforts(extra), default_effort: Some(default.into()) };
    vec![
        T3Instance { instance: T3_DEFAULT_INSTANCE.into(), name: "Claude code 2".into(), driver: "claudeAgent".into(), models: vec![
            claude("claude-opus-5-5", "Claude Opus 5.5", true, "medium"),
            claude("claude-sonnet-5-5", "Claude Sonnet 5.5", false, "high"),
            claude("claude-fable-5-1", "Claude Fable 5.1", true, "medium"),
            claude("claude-haiku-5-5", "Claude Haiku 5.5", false, "medium"),
        ] },
        T3Instance { instance: "codex".into(), name: "Codex".into(), driver: "codex".into(), models: vec![
            T3Model { id: "gpt-6-luna".into(), name: "GPT-6 Luna".into(), efforts: ["low", "medium", "high", "xhigh", "max"].map(String::from).to_vec(), default_effort: Some("medium".into()) },
        ] },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn haiku() -> (String, String) { (BACKGROUND_DEFAULT.0.into(), BACKGROUND_DEFAULT.1.into()) }

    #[test]
    fn providers_say_how_they_are_reached_in_the_menus_order() {
        let api = crate::demo_all_providers(ProviderAuth::Connected);
        let engine = EngineStatus { model_id: "qwen".into(), models: vec![EngineModel { id: "qwen".into(), label: "Qwen3.8 27B".into(), ..Default::default() }], ..Default::default() };
        let all = providers(&api, &engine, &demo_t3(), &haiku());
        let titles: Vec<(String, Option<String>, Access)> = all.iter().map(|p| (p.title.clone(), p.via.clone(), p.access)).collect();
        let at = |t: &str| titles.iter().position(|(x, ..)| x == t).unwrap_or_else(|| panic!("{t} missing from {titles:?}"));
        assert_eq!(titles[0], (LOCAL_ENGINE_TITLE.to_string(), None, Access::ThisMac));
        assert_eq!(titles[at("Claude Code subscription")].1.as_deref(), Some("Claude code 2"));
        assert_eq!(titles[at("Codex subscription")].1.as_deref(), Some("T3 Code"), "the T3 Codex instance, named once");
        assert!(at("Claude Code subscription") < at("Anthropic API") && at("OpenCode Go subscription") < at("Anthropic API"));
        assert!(all.iter().all(|p| p.title.contains(p.access.label()) || p.via.as_deref() == Some("this Mac") || p.id == LOCAL_ENGINE), "{titles:?}");
        let t3 = all.iter().find(|p| p.id == "t3:claudeAgent_claude_code_2").unwrap();
        let names: Vec<&str> = t3.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Claude Opus 5.5", "Claude Sonnet 5.5", "Claude Fable 5.1", "Claude Haiku 5.5"]);
        assert_eq!(t3.models[3].levels.last().map(String::as_str), Some("ultrathink"));
    }

    #[test]
    fn haiku_on_anthropic_api_is_always_listed_and_a_missing_key_says_so() {
        let api = crate::demo_anthropic_providers(ProviderAuth::Missing);
        let all = providers(&api, &EngineStatus::default(), &[], &haiku());
        let anthropic = all.iter().find(|p| p.id == "anthropic").unwrap();
        assert_eq!(anthropic.reason.as_deref(), Some("Not connected"));
        assert!(anthropic.models.iter().any(|m| m.id == BACKGROUND_DEFAULT.1));
        // Nothing listed by the app at all: the default still shows.
        let all = providers(&ApiProviders::default(), &EngineStatus::default(), &[], &haiku());
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].models[0].name, "Claude Haiku 5.5");
        // A connected provider without models is left out; a missing one is not offered.
        assert!(!providers(&crate::demo_anthropic_providers(ProviderAuth::Connected), &EngineStatus::default(), &[], &haiku()).iter().any(|p| p.id == "openai"));
    }

    #[test]
    fn a_t3_choice_t3_has_not_listed_stays_with_its_reason() {
        let chosen = ("t3:".to_string(), "claude-haiku-5-5".to_string());
        let all = providers(&ApiProviders::default(), &EngineStatus::default(), &[], &chosen);
        let t3 = all.iter().find(|p| p.id == "t3:claudeAgent_claude_code_2").unwrap();
        assert_eq!((t3.reason.as_deref(), t3.via.as_deref()), (Some("T3 Code not running"), Some("Claude code 2")));
        // Listed: no stand-in row, the real one.
        let all = providers(&ApiProviders::default(), &EngineStatus::default(), &demo_t3(), &chosen);
        assert_eq!(all.iter().filter(|p| p.id.starts_with(T3_PROVIDER_PREFIX)).count(), 2);
    }

    #[test]
    fn the_caption_names_the_t3_instance_and_the_chosen_level() {
        let mut settings = Settings { background_model: ("t3:claudeAgent_claude_code_2".into(), "claude-haiku-5-5".into()), ..Default::default() };
        let api = ApiProviders::default();
        assert_eq!(caption(&api, &EngineStatus::default(), &demo_t3(), &settings), "Claude Haiku 5.5 · Claude code 2");
        settings.background_efforts.push(("t3:claudeAgent_claude_code_2".into(), "claude-haiku-5-5".into(), "low".into()));
        assert_eq!(caption(&api, &EngineStatus::default(), &demo_t3(), &settings), "Claude Haiku 5.5 · Claude code 2 · Low");
        // `t3:` alone is the default instance, for the level too.
        settings.background_model.0 = "t3:".into();
        assert_eq!(caption(&api, &EngineStatus::default(), &demo_t3(), &settings), "Claude Haiku 5.5 · Claude code 2 · Low");
        // An API choice keeps the menu's naming (every provider says API or subscription, menu branch 30 Oct); its level
        // follows.
        let settings = Settings { background_efforts: vec![("anthropic".into(), "claude-haiku-5-5".into(), "xhigh".into())], ..Default::default() };
        assert_eq!(caption(&crate::demo_anthropic_providers(ProviderAuth::Connected), &EngineStatus::default(), &[], &settings), "Claude Haiku 5.5 · Anthropic API · XHigh");
    }

    #[test]
    fn the_default_model_serves_the_tasks_without_their_own_model() {
        // Bug note 2026-11-03T17-37-04: Islands and Goals on their own Haiku, Voice Names off; the rest inherit.
        let own = |t| (t, ISLANDS_DEFAULT.0.to_string(), ISLANDS_DEFAULT.1.to_string());
        let settings = Settings { task_models: vec![own(BackgroundTask::Islands), own(BackgroundTask::Goals)], background_off: vec![BackgroundTask::VoiceNames], ..Default::default() };
        assert_eq!(inheriting(&settings), [BackgroundTask::Plan, BackgroundTask::DayTimeline, BackgroundTask::Suggestions]);
        // An own entry with no provider is "Use Default Model": it inherits.
        let settings = Settings { task_models: vec![(BackgroundTask::Plan, String::new(), String::new())], ..Default::default() };
        assert!(inheriting(&settings).contains(&BackgroundTask::Plan));
    }

    #[test]
    fn levels_read_as_words() {
        let titles: Vec<String> = ["off", "low", "xhigh", "max", "ultracode", "ultrathink"].iter().map(|l| level_title(l)).collect();
        assert_eq!(titles, ["Off", "Low", "XHigh", "Max", "Ultracode", "Ultrathink"]);
    }
}
