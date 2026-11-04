//! Background Tasks in emergency mode (docs/claude-relay.md): the task rows greyed, one large
//! warning triangle centred over them, its tooltip the only words. One menu item holds the greyed rows and the
//! triangle; it stands in for the live toggles while emergency mode is on, so the normal rows stay as they are.
use objc2::rc::Retained;
use objc2::MainThreadOnly as _;
use objc2_app_kit::{NSAccessibility as _, NSAutoresizingMaskOptions, NSColor, NSImageSymbolConfiguration, NSImageView, NSMenuItem};
use objc2_foundation::{MainThreadMarker, NSArray};

use super::controls::CheckRow;
use super::util::{ns, rect, FlippedView};
use crate::model::BackgroundTask;

/// The triangle's symbol size: three rows tall at most, so it reads as one mark over the section.
const TRIANGLE_POINTS: f64 = 38.0;
const ROW: f64 = 24.0;

pub(crate) struct Paused {
    pub item: Retained<NSMenuItem>,
    pub rows: Vec<(BackgroundTask, Retained<CheckRow>)>,
    pub triangle: Retained<NSImageView>,
}

/// The hidden stand-in item (`sync` shows it in emergency mode).
pub(crate) fn item(off: &[BackgroundTask], mtm: MainThreadMarker) -> Paused {
    let rows: Vec<(BackgroundTask, Retained<CheckRow>)> = BackgroundTask::ALL.into_iter().map(|task| {
        // Row index usize::MAX: never a click target (dimmed rows take no clicks anyway).
        let row = CheckRow::new(usize::MAX, task.title(), Some(task.icon()), !off.contains(&task), mtm);
        row.set_dimmed(true);
        // The task rows open submenus (climb 2): their stand-ins show no switch, which would read as on.
        row.set_plain(true);
        (task, row)
    }).collect();
    let width = rows.iter().map(|(_, r)| r.frame().size.width).fold(200.0, f64::max).ceil();
    let height = ROW * rows.len() as f64;
    let host = FlippedView::new(rect(0.0, 0.0, width, height), mtm);
    host.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
    for (k, (_, row)) in rows.iter().enumerate() {
        row.setFrame(rect(0.0, k as f64 * ROW, width, ROW));
        row.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
        host.addSubview(row);
    }
    let triangle = NSImageView::initWithFrame(NSImageView::alloc(mtm), rect(0.0, 0.0, 0.0, 0.0));
    if let Some(image) = super::util::symbol(crate::island::object::EMERGENCY_SYMBOL) {
        // The exclamation mark in black on the triangle's orange: the island's emergency token, larger.
        let size = NSImageSymbolConfiguration::configurationWithPointSize_weight(TRIANGLE_POINTS, unsafe { objc2_app_kit::NSFontWeightRegular });
        let colours = NSImageSymbolConfiguration::configurationWithPaletteColors(&NSArray::from_retained_slice(&[NSColor::blackColor(), NSColor::systemOrangeColor()]));
        if let Some(image) = image.imageWithSymbolConfiguration(&size.configurationByApplyingConfiguration(&colours)) {
            let s = image.size();
            triangle.setImage(Some(&image));
            triangle.setFrame(rect(((width - s.width) / 2.0).round(), ((height - s.height) / 2.0).round(), s.width.ceil(), s.height.ceil()));
        }
    }
    // Centred however wide the menu lays the item.
    triangle.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMaxXMargin);
    host.addSubview(&triangle);
    let item = NSMenuItem::new(mtm);
    item.setTitle(&ns(super::menu::BACKGROUND_TITLE));
    item.setEnabled(false);
    item.setView(Some(&host));
    item.setHidden(true);
    Paused { item, rows, triangle }
}

/// Shows the stand-in for `relay` in emergency mode (with its tooltip) and the live toggles otherwise.
/// `values`: each task's model as its row names it, kept on the greyed stand-in (climb 3 Nov round 1: paused rows that
/// dropped their models no longer said what would run).
pub(crate) fn sync(paused: &Paused, live: &[(BackgroundTask, Retained<CheckRow>)], off: &[BackgroundTask], relay: &crate::model::RelayIndicator, values: &dyn Fn(BackgroundTask) -> Option<String>) {
    let tip = relay.paused_tooltip();
    for (_, row) in live {
        if let Some(item) = row.enclosingMenuItem() { item.setHidden(tip.is_some()); }
    }
    for (task, row) in &paused.rows {
        row.set_on(!off.contains(task));
        let value = values(*task);
        row.set_status(value.as_deref().map(|v| (v, super::controls::StatusTone::Grey)), None);
    }
    paused.item.setHidden(tip.is_none());
    paused.triangle.setToolTip(tip.as_deref().map(ns).as_deref());
    paused.triangle.setAccessibilityLabel(tip.as_deref().map(ns).as_deref());
}
