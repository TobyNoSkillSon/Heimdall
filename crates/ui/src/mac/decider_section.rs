//! The menu's Decider section ("a Decider section in the menu, one more collapsible section, where
//! we choose the quantization, so we can hot-swap"). Its header folds like the others (a fork's symbol, "Decider", the
//! chevron) and says at its trailing edge which precision runs: grey at rest, with a spinner while it loads or swaps
//! (until the new decider has answered), red "Failed" when nothing runs. Under it the version ("Decider V5") as the other
//! sections' captions, a line with a failure's reason (red, whole) or a missing folder's stand-in (orange), then the
//! precisions, largest first, each with one instance's memory: the one running rests on the light fill. A click on
//! another precision only marks it (the soft accent) and shows the glowing Confirm under the list; Confirm swaps. A click
//! on the current one, Escape or closing the menu cancels. Nothing is clickable while a swap runs, nor a precision
//! whose folder is missing.

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::sel;
use objc2_app_kit::{NSColor, NSMenu, NSMenuItem};
use objc2_foundation::MainThreadMarker;

use super::controls::{CheckRow, LabelRow, StatusTone};
use super::prominent::ProminentRow;
use super::util::*;
use crate::model::{DeciderState, DeciderStatus, UiAction};

/// Where the rows' text starts (the title column), as the other sections' captions and rows.
const TITLE_X: f64 = 39.0;

/// What the section shows for a status and a pending pick.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Look {
    /// The header's words and tone, and whether its spinner turns.
    pub header: (String, StatusTone),
    pub spinning: bool,
    /// The line under the version: a reason (red) or a missing folder's stand-in (orange).
    pub line: Option<(String, bool)>,
    /// The precision resting on the light fill: the one running (or swapping to, or that would load).
    pub lit: Option<String>,
    /// The precision marked for Confirm.
    pub pending: Option<String>,
    /// Per option (in `DeciderStatus::options` order): clickable.
    pub enabled: Vec<bool>,
}

pub(crate) fn look(d: &DeciderStatus, pending: Option<&str>) -> Look {
    let swapping = matches!(d.state, DeciderState::Swapping { .. });
    let held = d.running.clone().or_else(|| d.fallback.clone()).unwrap_or_else(|| d.chosen.clone());
    let (header, spinning, lit) = match &d.state {
        DeciderState::Swapping { to } => ((d.label(to), StatusTone::Grey), true, Some(to.clone())),
        DeciderState::Loading => ((d.label(&held), StatusTone::Grey), true, Some(held)),
        DeciderState::Failed(_) => (("Failed".to_string(), StatusTone::Alert), false, None),
        DeciderState::Ready | DeciderState::Off => ((d.label(&held), StatusTone::Grey), false, Some(held)),
    };
    let line = match (&d.reason, &d.fallback) {
        (Some(why), _) => Some((why.clone(), true)),
        (None, Some(used)) => Some((format!("{} is not on this Mac \u{b7} {} runs", d.label(&d.chosen), d.label(used)), false)),
        (None, None) => None,
    };
    let pending = pending.filter(|_| !swapping).map(str::to_string);
    let enabled = d.options.iter().map(|o| o.present && !swapping).collect();
    Look { header, spinning, line, lit, pending, enabled }
}

/// A click on precision `key`: the pending pick it leaves. The current choice (or what runs in its place) clears it;
/// another present one is marked; while a swap runs nothing is.
pub(crate) fn click(d: &DeciderStatus, key: &str) -> Option<String> {
    let current = look(d, None).lit;
    let ok = d.option(key).is_some_and(|o| o.present) && !matches!(d.state, DeciderState::Swapping { .. });
    (ok && current.as_deref() != Some(key) && key != d.chosen).then(|| key.to_string())
}

/// The section's live rows (the header is the section's, `State::sections`).
#[derive(Default)]
pub(crate) struct Rows {
    pub header: Option<Retained<CheckRow>>,
    pub version: Option<(Retained<NSMenuItem>, Retained<LabelRow>)>,
    pub line: Option<(Retained<NSMenuItem>, Retained<LabelRow>)>,
    /// One row per precision: the item, its row, its key.
    pub options: Vec<(Retained<NSMenuItem>, Retained<ProminentRow>, String)>,
    pub confirm: Option<(Retained<NSMenuItem>, Retained<super::model_choice::ConfirmView>)>,
    /// The option keys the rows were laid for (a new set lays the menu again).
    pub keys: Vec<String>,
}

impl Rows {
    pub(crate) fn items(&self) -> Vec<Retained<NSMenuItem>> {
        let mut out: Vec<Retained<NSMenuItem>> = self.version.iter().chain(self.line.iter()).map(|(i, _)| i.clone()).collect();
        out.extend(self.options.iter().map(|(i, _, _)| i.clone()));
        out.extend(self.confirm.iter().map(|(i, _)| i.clone()));
        out
    }
}

/// The section's rows under its header, appended to `menu`; synced at once.
pub(crate) fn add_rows(state: &mut super::State, menu: &NSMenu, header: Retained<CheckRow>, target: Option<&AnyObject>, mtm: MainThreadMarker) {
    let (version, row) = super::menu::label_item(TITLE_X, mtm);
    menu.addItem(&version);
    let version = (version, row);
    let (line, row) = super::menu::label_item(TITLE_X, mtm);
    row.set_wraps();
    menu.addItem(&line);
    let line = (line, row);
    let mut options = vec![];
    for (i, o) in state.decider.options.iter().enumerate() {
        let item = menu_item(&o.label, None, Some(sel!(pickDeciderPrecision:)), "", target, mtm);
        item.setTag(i as isize);
        let view = ProminentRow::in_place(&item, mtm);
        view.set_detail(Some(&super::setup::size_text(o.memory_bytes)));
        view.setToolTip(Some(&ns(&format!("{} memory \u{b7} {} on disk", super::setup::size_text(o.memory_bytes), super::setup::size_text(o.disk_bytes)))));
        item.setView(Some(&view));
        menu.addItem(&item);
        options.push((item, view, o.key.clone()));
    }
    let width = options.iter().map(|(_, v, _)| v.frame().size.width).fold(super::menu::MIN_SUBMENU, f64::max);
    let (confirm, view) = super::model_choice::confirm_item_for(width, true, mtm);
    menu.addItem(&confirm);
    let keys = state.decider.options.iter().map(|o| o.key.clone()).collect();
    state.decider_rows = Rows { header: Some(header), version: Some(version), line: Some(line), options, confirm: Some((confirm, view)), keys };
    sync(state);
}

/// Everything that changes in place: the header, the version, the line, the lit and pending rows, Confirm.
pub(crate) fn sync(state: &super::State) {
    let d = &state.decider;
    let rows = &state.decider_rows;
    let l = look(d, state.decider_pending.as_deref());
    // The version goes on the header beside the precision ("V5 · 4-bit"), not on a line of its own under it (climb 2:
    // the line read as a subheading and cost a row).
    if let Some(header) = &rows.header {
        let short = d.version.strip_prefix("Decider ").unwrap_or(&d.version);
        // Open, its lit row names the precision: the header keeps the version alone (climb 2: no value said twice).
        let open = !state.settings_value.collapsed.contains(&crate::model::MenuSection::Decider);
        let text = if short.is_empty() || l.header.1 != StatusTone::Grey { l.header.0.clone() } else if open { short.to_string() } else { format!("{short} \u{b7} {}", l.header.0) };
        header.set_status(Some((&text, l.header.1)), l.spinning.then_some(None));
        header.setToolTip((!d.version.is_empty()).then(|| super::util::ns(&d.version)).as_deref());
    }
    if let Some((item, row)) = &rows.version {
        row.set(&d.version, false, false);
        item.setHidden(true);
    }
    if let Some((item, row)) = &rows.line {
        match &l.line {
            Some((text, true)) => { row.set_failure(text); item.setHidden(false); }
            Some((text, false)) => { row.set(text, true, false); item.setHidden(false); }
            None => item.setHidden(true),
        }
    }
    for (n, (item, row, key)) in rows.options.iter().enumerate() {
        let enabled = l.enabled.get(n).copied().unwrap_or(false);
        let pending = l.pending.as_deref() == Some(key.as_str());
        item.setEnabled(enabled);
        row.set_resting(pending || l.lit.as_deref() == Some(key.as_str()));
        let ink = (!enabled).then(NSColor::tertiaryLabelColor);
        let fill = pending.then(|| NSColor::controlAccentColor().colorWithAlphaComponent(0.16));
        row.set_tint(ink, fill);
    }
    if let Some((item, _)) = &rows.confirm { item.setHidden(l.pending.is_none()); }
}

impl super::State {
    /// Decider ▸ precision row `index` clicked: marked for Confirm, or the mark cleared (`click`). Nothing reloads.
    pub(crate) fn pick_decider(&mut self, index: usize) {
        let Some(key) = self.decider.options.get(index).map(|o| o.key.clone()) else { return };
        self.decider_pending = click(&self.decider, &key);
        self.refresh_decider();
    }

    /// Confirm: the pending precision goes to the app, and the section shows the swap at once (the app's status follows).
    pub(crate) fn confirm_decider(&mut self) {
        let Some(key) = self.decider_pending.take() else { return };
        self.emit(UiAction::SetDeciderPrecision(key.clone()));
        let loaded = matches!(self.decider.state, DeciderState::Ready | DeciderState::Loading);
        if loaded { self.decider.state = DeciderState::Swapping { to: key.clone() }; }
        self.decider.chosen = key;
        self.decider.reason = None;
        self.decider.fallback = None;
        self.refresh_decider();
    }

    /// The section synced in place in an open menu (folded sections stay folded); a closed one rebuilds when it opens.
    pub(crate) fn refresh_decider(&mut self) {
        if !self.menu_open && !super::menu_on_screen(self.mtm) {
            self.menu_stale = true;
            return;
        }
        super::menu::sync_tracking(self);
    }
}

/// The header's spinner (stopped when the menu closes, started when it opens).
pub(crate) fn indicators(state: &super::State) -> Vec<Retained<objc2_app_kit::NSProgressIndicator>> {
    state.decider_rows.header.as_ref().and_then(|h| h.status_spinner()).into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DeciderOption;

    pub(crate) fn status(state: DeciderState, chosen: &str, running: Option<&str>) -> DeciderStatus {
        let o = |key: &str, label: &str, mem: u64| DeciderOption { key: key.into(), label: label.into(), disk_bytes: mem / 2, memory_bytes: mem, present: true };
        DeciderStatus {
            version: "Decider V5".into(),
            options: vec![o("16bit", "16-bit", 4_160_000_000), o("8bit", "8-bit", 3_080_000_000), o("4bit", "4-bit", 2_350_000_000)],
            chosen: chosen.into(),
            running: running.map(Into::into),
            fallback: None,
            reason: None,
            state,
        }
    }

    #[test]
    fn at_rest_the_running_precision_is_lit_and_named_in_the_header() {
        let l = look(&status(DeciderState::Ready, "4bit", Some("4bit")), None);
        assert_eq!(l.header, ("4-bit".into(), StatusTone::Grey));
        assert!(!l.spinning && l.line.is_none() && l.pending.is_none());
        assert_eq!(l.lit.as_deref(), Some("4bit"));
        assert_eq!(l.enabled, [true, true, true]);
        // Off (Listening off): what would load.
        assert_eq!(look(&status(DeciderState::Off, "8bit", None), None).lit.as_deref(), Some("8bit"));
    }

    #[test]
    fn a_click_marks_another_precision_and_the_current_one_cancels() {
        let d = status(DeciderState::Ready, "4bit", Some("4bit"));
        assert_eq!(click(&d, "8bit").as_deref(), Some("8bit"));
        assert_eq!(click(&d, "4bit"), None, "the current one cancels");
        let l = look(&d, Some("8bit"));
        assert_eq!((l.pending.as_deref(), l.lit.as_deref()), (Some("8bit"), Some("4bit")), "marked, nothing reloaded");
        let mut missing = d.clone();
        missing.options[0].present = false;
        assert_eq!(click(&missing, "16bit"), None, "a missing folder is not a pick");
        assert_eq!(look(&missing, None).enabled, [false, true, true]);
    }

    #[test]
    fn swapping_spins_on_the_target_and_takes_no_clicks() {
        let d = status(DeciderState::Swapping { to: "8bit".into() }, "8bit", Some("4bit"));
        let l = look(&d, Some("16bit"));
        assert_eq!(l.header, ("8-bit".into(), StatusTone::Grey));
        assert!(l.spinning);
        assert_eq!(l.lit.as_deref(), Some("8bit"));
        assert_eq!(l.pending, None);
        assert_eq!(l.enabled, [false, false, false]);
        assert_eq!(click(&d, "16bit"), None);
    }

    #[test]
    fn failures_and_a_missing_folder_say_why_under_the_version() {
        let mut d = status(DeciderState::Ready, "4bit", Some("4bit"));
        d.reason = Some("8-bit didn\u{2019}t load: no Metal device".into());
        let l = look(&d, None);
        assert_eq!(l.header.0, "4-bit", "rolled back: what runs, in grey");
        assert_eq!(l.line, Some(("8-bit didn\u{2019}t load: no Metal device".into(), true)));
        let failed = look(&status(DeciderState::Failed("no Metal device".into()), "4bit", None), None);
        assert_eq!(failed.header, ("Failed".into(), StatusTone::Alert));
        assert_eq!(failed.lit, None);
        let mut fb = status(DeciderState::Ready, "4bit", Some("8bit"));
        fb.fallback = Some("8bit".into());
        fb.options[2].present = false;
        let l = look(&fb, None);
        assert_eq!(l.line, Some(("4-bit is not on this Mac \u{b7} 8-bit runs".into(), false)));
        assert_eq!((l.header.0.as_str(), l.lit.as_deref()), ("8-bit", Some("8bit")));
    }
}
