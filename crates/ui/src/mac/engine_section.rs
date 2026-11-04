//! The menu's Engine section ("Move all local-model management out of the agent's model
//! picker into an Engine section of its own"). Its header is a section disclosure like the others (the engine's `cpu`
//! symbol, "Engine", the chevron) and says at its trailing edge what the engine does now, the tile's states in words:
//! Off, Starting, Loading (a spinner, or a filling ring while Splash reports a download's percentage), Ready, Failed
//! (red, with the reason on the line under it, whole), and, on with no model loaded, "Pick a model" in the soft accent.
//! Under it, only the models already on this Mac, one row per precision: a click loads it (the engine turns on; the
//! agent's model never changes), and the loaded one rests on the lighter fill Support the Developer has. One loads at
//! a time. A download in flight shows here too, greyed, with a small spinner and a bar, and joins the list when it lands.
//! Last, Add Model ▸: every model, each opening to its precisions (`engine_rows`): a click downloads one, the trash
//! deletes one. Off, the rows are grey; waiting for a pick, they rest on the soft accent the tile shows.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{sel, MainThreadOnly};
use objc2_app_kit::{NSColor, NSMenu, NSMenuItem, NSProgressIndicator};
use objc2_foundation::MainThreadMarker;

use super::controls::{self, CheckRow, LabelRow, StatusTone};
use super::engine_rows::{self, PrecisionRow};
use super::prominent::ProminentRow;
use super::util::*;
use crate::model::{EngineState, EngineStatus};

pub(crate) const ADD_MODEL: &str = "Add Model";
const ADD_MODEL_ICON: &str = "plus.circle";

/// A quick-pick row: on this Mac, or downloading (its fraction when the size is known).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PickKind {
    OnDisk,
    Downloading(Option<f64>),
}

/// One quick-pick row: a model's precision on this Mac (or on its way).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PickEntry {
    pub id: String,
    pub label: String,
    pub quant: String,
    pub kind: PickKind,
    /// The engine holds it (loaded or loading), or, off, would load it.
    pub loaded: bool,
    /// It fits the engine's memory ceiling (one over it is listed, greyed: its download was asked for, not a load).
    pub fits: bool,
}

/// The quick-pick list, in the catalog's order: every precision on this Mac, and the one downloading.
pub(crate) fn picks(engine: &EngineStatus) -> Vec<PickEntry> {
    let mut out = vec![];
    for m in &engine.models {
        for q in &m.quants {
            let downloading = m.download.as_ref().filter(|d| d.quant == q.quant && d.error.is_none() && !q.downloaded);
            if !q.downloaded && downloading.is_none() { continue }
            let kind = match downloading {
                Some(d) => PickKind::Downloading(d.total_bytes.filter(|t| *t > 0).map(|t| d.done_bytes as f64 / t as f64)),
                None => PickKind::OnDisk,
            };
            let loaded = kind == PickKind::OnDisk && is_loaded(engine, &m.id, &q.quant);
            let fits = engine_rows::over_limit(q, engine.memory_limit_bytes).is_none();
            out.push(PickEntry { id: m.id.clone(), label: m.label.clone(), quant: q.quant.clone(), kind, loaded, fits });
        }
    }
    out
}

/// Whether the engine holds this precision: its model at its chosen precision, unless the engine waits for a pick.
pub(crate) fn is_loaded(engine: &EngineStatus, id: &str, quant: &str) -> bool {
    engine.state != EngineState::Waiting && !engine.model_id.is_empty() && engine.model_id == id
        && engine.models.iter().any(|m| m.id == id && m.quant == quant)
}

/// The header's live status: its words, their tone, and a spinner (`Some(None)`) or a ring (`Some(Some(f))`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Status {
    pub text: String,
    pub tone: StatusTone,
    pub progress: Option<Option<f64>>,
}

pub(crate) fn status(engine: &EngineStatus) -> Status {
    let s = |text: &str, tone, progress| Status { text: text.into(), tone, progress };
    match &engine.state {
        EngineState::Off => s("Off", StatusTone::Grey, None),
        EngineState::Starting => s("Starting", StatusTone::Grey, Some(None)),
        EngineState::Loading { percent: None } => s("Loading", StatusTone::Grey, Some(None)),
        EngineState::Loading { percent: Some(p) } => Status { text: format!("Loading {p}%"), tone: StatusTone::Grey, progress: Some(Some(f64::from(*p) / 100.0)) },
        EngineState::Waiting => s("Pick a model", StatusTone::Accent, None),
        EngineState::Ready | EngineState::Idle => s("Ready", StatusTone::Grey, None),
        EngineState::Error(_) => s("Failed", StatusTone::Alert, None),
    }
}

/// The failure's reason, shown whole under the header (not only in a tooltip).
pub(crate) fn reason(engine: &EngineStatus) -> Option<String> {
    match &engine.state { EngineState::Error(why) => Some(why.clone()), _ => None }
}

/// How the section's rows rest: grey while the engine is off, the soft accent while it waits for a pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone { Plain, Grey, Waiting }

pub(crate) fn tone(engine: &EngineStatus) -> Tone {
    match engine.state { EngineState::Off => Tone::Grey, EngineState::Waiting => Tone::Waiting, _ => Tone::Plain }
}

/// What the rows were laid from: a change here lays them again; anything else (progress, the loaded one, the tone)
/// syncs in place.
pub(crate) type Shape = (Vec<(String, String, bool)>, Vec<(String, Vec<bool>, Option<(String, Option<String>, bool)>)>, Vec<String>);

pub(crate) fn shape(engine: &EngineStatus, armed: &[String]) -> Shape {
    let picks = picks(engine).into_iter().map(|p| (p.id, p.quant, p.kind == PickKind::OnDisk)).collect();
    let models = engine.models.iter().map(|m| (m.id.clone(), m.quants.iter().map(|q| q.downloaded).collect(),
        m.download.as_ref().map(|d| (d.quant.clone(), d.error.clone(), d.total_bytes.is_some())))).collect();
    (picks, models, armed.to_vec())
}

/// A quick-pick row's live parts.
pub(crate) struct Pick {
    pub item: Retained<NSMenuItem>,
    pub row: Retained<ProminentRow>,
    pub entry: PickEntry,
    pub bar: Option<Retained<NSProgressIndicator>>,
}

/// The section's live rows (the header is the section's, `State::sections`).
#[derive(Default)]
pub(crate) struct Rows {
    pub header: Option<Retained<CheckRow>>,
    pub reason: Option<(Retained<NSMenuItem>, Retained<LabelRow>)>,
    pub picks: Vec<Pick>,
    pub add_model: Option<Retained<NSMenuItem>>,
    /// Add Model ▸ each model's row (its submenu holds the precisions), by model id.
    pub models: Vec<(Retained<NSMenuItem>, String)>,
    /// Every precision row under Add Model (a row's tag indexes this list).
    pub precisions: Vec<PrecisionRow>,
    pub shape: Option<Shape>,
}

impl Rows {
    /// The section's items in menu order, under the header.
    pub(crate) fn items(&self) -> Vec<Retained<NSMenuItem>> {
        self.reason.iter().map(|(i, _)| i.clone()).chain(self.picks.iter().map(|p| p.item.clone())).chain(self.add_model.clone()).collect()
    }
}

/// Where the rows' text starts (a native row's title column).
const TITLE_X: f64 = 39.0;
/// A downloading row's bar.
const BAR_WIDTH: f64 = 64.0;

/// The precisions whose trash was pressed once ("model id:quant"), from the menu's expanded set.
pub(crate) fn armed(expanded: &std::collections::BTreeSet<(String, String)>) -> Vec<String> {
    expanded.iter().filter(|(k, _)| k == super::menu::TRASH_ARMED).map(|(_, v)| v.clone()).collect()
}

/// The reason line and quick-pick rows, built fresh (in the menu's order), with the state they show.
fn pick_rows(engine: &EngineStatus, target: Option<&AnyObject>, mtm: MainThreadMarker) -> ((Retained<NSMenuItem>, Retained<LabelRow>), Vec<Pick>) {
    let (line, row) = super::menu::label_item(TITLE_X, mtm);
    row.set_wraps();
    let mut out = vec![];
    for (i, e) in picks(engine).into_iter().enumerate() {
        let item = menu_item(&e.label, None, Some(sel!(loadEnginePick:)), "", target, mtm);
        item.setTag(i as isize);
        let view = ProminentRow::in_place(&item, mtm);
        view.set_detail(Some(&e.quant));
        let mut bar = None;
        if let PickKind::Downloading(done) = &e.kind {
            // Downloading: a small spinner in the icon column, a bar at the trailing edge; not a pick yet.
            item.setEnabled(false);
            let s = controls::spinner(12.0, false, mtm);
            s.setHidden(controls::STILL_SPINNERS.with(|c| c.get()));
            controls::set_progress(&s, None);
            s.setFrame(rect(16.0, 6.0, 12.0, 12.0));
            view.addSubview(&s);
            let b = NSProgressIndicator::initWithFrame(NSProgressIndicator::alloc(mtm), rect(0.0, 0.0, BAR_WIDTH, 6.0));
            b.setStyle(objc2_app_kit::NSProgressIndicatorStyle::Bar);
            b.setControlSize(objc2_app_kit::NSControlSize::Small);
            b.setMinValue(0.0);
            b.setMaxValue(1.0);
            b.setIndeterminate(done.is_none());
            b.setDoubleValue(done.unwrap_or(0.0));
            let w = view.frame().size.width + BAR_WIDTH + 12.0;
            view.setFrameSize(objc2_foundation::NSSize::new(w, view.frame().size.height));
            b.setFrame(rect(w - 16.0 - BAR_WIDTH, 6.0, BAR_WIDTH, 12.0));
            b.setAutoresizingMask(objc2_app_kit::NSAutoresizingMaskOptions::ViewMinXMargin);
            view.addSubview(&b);
            bar = Some(b);
        } else if !e.fits {
            item.setEnabled(false);
        }
        item.setView(Some(&view));
        out.push(Pick { item, row: view, entry: e, bar });
    }
    ((line, row), out)
}

/// Add Model ▸: every catalog model, each opening to its precisions; the rows and the precisions they hold.
fn add_model_menu(engine: &EngineStatus, armed: &[String], target: Option<&AnyObject>, mtm: MainThreadMarker)
    -> (Retained<NSMenu>, Vec<(Retained<NSMenuItem>, String)>, Vec<PrecisionRow>) {
    let sub = NSMenu::new(mtm);
    sub.setAutoenablesItems(false);
    let mut models = vec![];
    let mut precisions = vec![];
    for m in &engine.models {
        let (own, rows) = model_menu(engine, m, armed, precisions.len(), target, mtm);
        precisions.extend(rows);
        let item = super::menu::submenu_item(&m.label, None, &own, mtm);
        set_download_badge(&item, m);
        sub.addItem(&item);
        models.push((item, m.id.clone()));
    }
    (sub, models, precisions)
}

/// One model's precisions (`engine_rows::precision_row`), tagged from `first` on.
fn model_menu(engine: &EngineStatus, m: &crate::model::EngineModel, armed: &[String], first: usize, target: Option<&AnyObject>, mtm: MainThreadMarker)
    -> (Retained<NSMenu>, Vec<PrecisionRow>) {
    let own = NSMenu::new(mtm);
    own.setAutoenablesItems(false);
    let limit = engine.memory_limit_bytes;
    // Room for a row's reason too (a failed download, a refused delete), as wide as the row shows it (`engine_rows::REASON_MAX` at most).
    let reason = m.download.as_ref().and_then(|d| d.error.as_deref()).map_or(0.0, |e| text_width(e, &menu_font()).min(engine_rows::REASON_MAX).ceil() + 10.0);
    let width = m.quants.iter().map(|q| engine_rows::row_width(q, limit) + reason).fold(super::menu::MIN_SUBMENU + 60.0, f64::max).ceil();
    let mut rows = vec![];
    for q in &m.quants {
        let is_armed = armed.iter().any(|a| *a == format!("{}:{}", m.id, q.quant));
        let row = engine_rows::precision_row(m, q, is_armed, limit, width, (first + rows.len()) as isize, target, mtm);
        own.addItem(&row.item);
        rows.push(row);
    }
    (own, rows)
}

/// A model downloading says how far on its Add Model row (the badge AppKit draws at the trailing edge); nothing at rest.
fn set_download_badge(item: &NSMenuItem, m: &crate::model::EngineModel) {
    use objc2::AllocAnyThread as _;
    let text = m.download.as_ref().filter(|d| d.error.is_none()).map(|d| match d.total_bytes.filter(|t| *t > 0) {
        Some(t) => format!("{}%", (d.done_bytes * 100 / t).min(100)),
        None => "\u{2026}".into(),
    });
    let badge = text.map(|t| objc2_app_kit::NSMenuItemBadge::initWithString(objc2_app_kit::NSMenuItemBadge::alloc(), &ns(&t)));
    item.setBadge(badge.as_deref());
}

/// The section's rows under its header, appended to `menu`; the header's status is set by `sync`.
pub(crate) fn add_rows(state: &mut super::State, menu: &NSMenu, header: Retained<CheckRow>, target: Option<&AnyObject>, mtm: MainThreadMarker) {
    let armed = armed(&state.api_expanded);
    let (reason, picks) = pick_rows(&state.engine, target, mtm);
    menu.addItem(&reason.0);
    for p in &picks { menu.addItem(&p.item); }
    let (sub, models, precisions) = add_model_menu(&state.engine, &armed, target, mtm);
    let add = super::menu::submenu_item(ADD_MODEL, Some(ADD_MODEL_ICON), &sub, mtm);
    show_menu_images(&sub);
    menu.addItem(&add);
    state.engine_rows = Rows { header: Some(header), reason: Some(reason), picks, add_model: Some(add), models, precisions, shape: Some(shape(&state.engine, &armed)) };
    sync(state);
}

/// Lays the rows again in place, in the open menu, when what they were laid from changed (a precision arrived, left,
/// started or ended a download, a trash was armed): the reason line and the quick-picks are new items where the old
/// ones were; Add Model and each model's submenu keep their menus, so one open under the pointer stays open. The
/// caller syncs afterwards (`menu::sync_tracking`).
pub(crate) fn relay(state: &mut super::State) {
    let armed = armed(&state.api_expanded);
    let next = shape(&state.engine, &armed);
    if state.engine_rows.shape.as_ref() == Some(&next) { return; }
    let Some(add) = state.engine_rows.add_model.clone() else { return };
    let menu = state.menu.clone();
    // Nothing is taken out unless the rows are where they belong.
    if menu.indexOfItem(&add) < 0 { return; }
    let mtm = state.mtm;
    let target = state.target();
    let target = Some(&*target);
    // Out with the old reason line and picks; in with the new ones where they were.
    let old: Vec<Retained<NSMenuItem>> = state.engine_rows.reason.iter().map(|(i, _)| i.clone()).chain(state.engine_rows.picks.iter().map(|p| p.item.clone())).collect();
    for item in &old { if menu.indexOfItem(item) >= 0 { menu.removeItem(item); } }
    let at = menu.indexOfItem(&add);
    if at < 0 { return; }
    let (reason, picks) = pick_rows(&state.engine, target, mtm);
    menu.insertItem_atIndex(&reason.0, at);
    for (n, p) in picks.iter().enumerate() { menu.insertItem_atIndex(&p.item, at + 1 + n as isize); }
    // Add Model: the same model rows when the catalog is the same, each submenu refilled; else refilled whole.
    let same = state.engine_rows.models.iter().map(|(_, id)| id.as_str()).eq(state.engine.models.iter().map(|m| m.id.as_str()));
    let mut precisions = vec![];
    if same {
        for ((item, _), m) in state.engine_rows.models.iter().zip(&state.engine.models) {
            let (fresh, rows) = model_menu(&state.engine, m, &armed, precisions.len(), target, mtm);
            super::menu::refill(item, &fresh);
            set_download_badge(item, m);
            precisions.extend(rows);
        }
    } else {
        let (fresh, models, rows) = add_model_menu(&state.engine, &armed, target, mtm);
        super::menu::refill(&add, &fresh);
        state.engine_rows.models = models;
        precisions = rows;
    }
    state.engine_rows.reason = Some(reason);
    state.engine_rows.picks = picks;
    state.engine_rows.precisions = precisions;
    state.engine_rows.shape = Some(next);
    // The section's item list follows (folding hides the new rows too).
    let items = state.engine_rows.items();
    if let Some((_, list)) = state.sections.get_mut(&crate::model::MenuSection::Engine) { *list = items; }
    show_menu_images(&menu);
}

/// Every progress indicator the menu holds now: the tiles' and the header's spinners, the quick-pick's spinners and
/// bars, the precisions' bars (stopped when the menu closes, started again when it opens).
pub(crate) fn indicators(state: &super::State) -> Vec<Retained<NSProgressIndicator>> {
    let rows = &state.engine_rows;
    let mut out: Vec<Retained<NSProgressIndicator>> = state.tile_views.iter().filter_map(|t| t.spinner()).collect();
    out.extend(rows.header.as_ref().and_then(|h| h.status_spinner()));
    for p in &rows.picks {
        out.extend(p.row.subviews().iter().filter_map(|v| v.downcast::<NSProgressIndicator>().ok()));
    }
    out.extend(rows.precisions.iter().filter_map(|p| p.progress.clone()));
    out
}

/// Everything that changes in place: the header's words and spinner, the reason line, which row is loaded, the rows'
/// tone, the bars.
pub(crate) fn sync(state: &super::State) {
    let engine = &state.engine;
    let rows = &state.engine_rows;
    if let Some(header) = &rows.header {
        let s = status(engine);
        header.set_status(Some((&s.text, s.tone)), s.progress);
    }
    if let Some((item, row)) = &rows.reason {
        match reason(engine) {
            Some(why) => { row.set_failure(&why); item.setHidden(false); }
            None => item.setHidden(true),
        }
    }
    let now = picks(engine);
    let tone = tone(engine);
    for p in &rows.picks {
        let entry = now.iter().find(|e| e.id == p.entry.id && e.quant == p.entry.quant);
        p.row.set_resting(entry.is_some_and(|e| e.loaded));
        let (ink, fill): (Option<Retained<NSColor>>, Option<Retained<NSColor>>) = match tone {
            Tone::Grey => (Some(NSColor::secondaryLabelColor()), None),
            Tone::Waiting => (None, Some(NSColor::controlAccentColor().colorWithAlphaComponent(0.16))),
            Tone::Plain => (None, None),
        };
        // Waiting: every pick rests on the soft accent (pick one); otherwise only the loaded one rests, on the light fill.
        if tone == Tone::Waiting { p.row.set_resting(true); }
        // Not a pick (on its way, or over the memory ceiling): greyed further, whatever the tone.
        let inert = entry.is_none_or(|e| e.kind != PickKind::OnDisk || !e.fits);
        let ink = if inert { Some(NSColor::tertiaryLabelColor()) } else { ink };
        p.row.set_tint(ink, fill);
        if let (Some(bar), Some(PickKind::Downloading(Some(done)))) = (&p.bar, entry.map(|e| &e.kind)) {
            bar.setIndeterminate(false);
            bar.setDoubleValue(*done);
        }
    }
    for p in &rows.precisions {
        if let (Some(bar), Some(d)) = (&p.progress, engine.models.iter().find(|m| m.id == p.id).and_then(|m| m.download.as_ref())) {
            if let Some(total) = d.total_bytes.filter(|t| *t > 0) { bar.setIndeterminate(false); bar.setDoubleValue(d.done_bytes as f64 / total as f64); }
        }
    }
    for ((item, id), m) in rows.models.iter().filter_map(|(i, id)| engine.models.iter().find(|m| &m.id == id).map(|m| ((i, id), m))) {
        let _ = id;
        set_download_badge(item, m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EngineDownload, EngineModel, EngineQuant};

    fn engine(state: EngineState, model_id: &str) -> EngineStatus {
        let q = |quant: &str, downloaded: bool, ram: u64| EngineQuant { quant: quant.into(), source: format!("x/y:{quant}"), size_bytes: Some(10), ram_bytes: Some(ram), downloaded };
        EngineStatus {
            state,
            model_id: model_id.into(),
            model_label: String::new(),
            models: vec![
                EngineModel { id: "qwen".into(), label: "Qwen".into(), quant: "Q4".into(), quants: vec![q("Q2", false, 1), q("Q4", true, 1), q("Q8", true, 1)], ..Default::default() },
                EngineModel { id: "flint".into(), label: "Flint".into(), quant: "Q4".into(), quants: vec![q("Q4", false, 1)],
                    download: Some(EngineDownload { quant: "Q4".into(), done_bytes: 5, total_bytes: Some(10), error: None }), ..Default::default() },
                EngineModel { id: "giant".into(), label: "Giant".into(), quant: "Q2".into(), quants: vec![q("Q2", true, 99)], ..Default::default() },
            ],
            memory_limit_bytes: Some(48),
            ..Default::default()
        }
    }

    /// only the models already downloaded are listed, plus the one on its way; the loaded one is marked.
    #[test]
    fn the_quick_pick_lists_what_is_on_this_mac_and_marks_the_loaded_one() {
        let e = engine(EngineState::Ready, "qwen");
        let p = picks(&e);
        let names: Vec<(&str, &str)> = p.iter().map(|p| (p.id.as_str(), p.quant.as_str())).collect();
        assert_eq!(names, [("qwen", "Q4"), ("qwen", "Q8"), ("flint", "Q4"), ("giant", "Q2")], "Q2 is not here; Flint's download is listed");
        assert_eq!(p.iter().filter(|p| p.loaded).map(|p| p.quant.as_str()).collect::<Vec<_>>(), ["Q4"], "the chosen precision of the loaded model");
        assert_eq!(p[2].kind, PickKind::Downloading(Some(0.5)));
        assert!(!p[3].fits, "over the ceiling: listed, not a pick");
        // Waiting for a pick: nothing is loaded even if a model id lingers.
        assert!(picks(&engine(EngineState::Waiting, "qwen")).iter().all(|p| !p.loaded));
        // A failed download is not a row here (Add Model says why).
        let mut failed = engine(EngineState::Ready, "qwen");
        failed.models[1].download.as_mut().unwrap().error = Some("Hub unreachable".into());
        assert!(picks(&failed).iter().all(|p| p.id != "flint"));
    }

    /// The header says every state in words, and only an on-its-way state animates.
    #[test]
    fn the_header_says_each_state_and_only_work_in_progress_spins() {
        let st = |s| status(&engine(s, "qwen"));
        assert_eq!(st(EngineState::Off), Status { text: "Off".into(), tone: StatusTone::Grey, progress: None });
        assert_eq!(st(EngineState::Starting).progress, Some(None));
        assert_eq!(st(EngineState::Loading { percent: None }), Status { text: "Loading".into(), tone: StatusTone::Grey, progress: Some(None) });
        assert_eq!(st(EngineState::Loading { percent: Some(40) }), Status { text: "Loading 40%".into(), tone: StatusTone::Grey, progress: Some(Some(0.4)) });
        assert_eq!(st(EngineState::Waiting), Status { text: "Pick a model".into(), tone: StatusTone::Accent, progress: None });
        assert_eq!(st(EngineState::Ready).text, "Ready");
        assert_eq!(st(EngineState::Error("Splash exited".into())), Status { text: "Failed".into(), tone: StatusTone::Alert, progress: None });
        assert_eq!(reason(&engine(EngineState::Error("Splash exited".into()), "qwen")).as_deref(), Some("Splash exited"));
        assert_eq!(reason(&engine(EngineState::Ready, "qwen")), None);
        assert_eq!(tone(&engine(EngineState::Off, "qwen")), Tone::Grey);
        assert_eq!(tone(&engine(EngineState::Waiting, "")), Tone::Waiting);
    }

    /// Progress alone never lays the rows again; a download ending (the precision on disk) does.
    #[test]
    fn progress_syncs_in_place_and_a_landing_relays() {
        let a = engine(EngineState::Ready, "qwen");
        let mut b = a.clone();
        b.models[1].download.as_mut().unwrap().done_bytes = 9;
        assert_eq!(shape(&a, &[]), shape(&b, &[]));
        b.models[1].download = None;
        b.models[1].quants[0].downloaded = true;
        assert_ne!(shape(&a, &[]), shape(&b, &[]));
        assert_ne!(shape(&a, &[]), shape(&a, &["qwen:Q8".into()]), "an armed trash lays its row again");
    }
}
