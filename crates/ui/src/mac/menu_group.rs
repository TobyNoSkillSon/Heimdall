//! An open section reads as one group (bug note 2026-11-03T17-37-05, the user: "I just clicked it and I didn't even
//! realise what was opened... we can just lighten it up a little bit when it's expanded"): its header and the rows it
//! shows sit on one gently lighter panel with rounded corners, inset like the pointer's highlight, its chevron pointing
//! down (`controls::CheckRow::disclosure`). A folded section has none.
//!
//! AppKit lays the menu's rows out in a table (`menu_motion`): the panel is drawn as a still layer at the back of each
//! row view of the group, so it rides with the rows when the menu moves them, sits under the pointer's highlight, and
//! needs no drawing at rest. It is set when AppKit lays a row view out (`NSTableRowView.layout`, wrapped once) and when
//! the sections change (`repaint`). The offscreen renders draw the same panel from the same groups (`offscreen::stacked`).

use std::cell::RefCell;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, Imp, Sel};
use objc2::{msg_send, sel};
use objc2_app_kit::{NSMenuItem, NSTableRowView, NSView, NSWindow};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_quartz_core::{CALayer, CATransaction};

/// The panel's inset from the menu's edges: the pointer's highlight's own, so the two line up.
pub(crate) const INSET: f64 = 5.0;
/// Its corners.
pub(crate) const RADIUS: f64 = 8.0;
/// Its fill over the menu's material, as (white, alpha): a little lighter in the dark look; in the light look, whose
/// material is near white already, a soft grey instead (a white panel there did not show, render 3 Nov).
pub(crate) fn fill(dark: bool) -> (f64, f64) {
    if dark { (1.0, 0.075) } else { (0.0, 0.05) }
}

const NAME: &str = "heimdall.section-group";

thread_local! {
    /// Each open section: its header's item, then its rows, in menu order.
    static GROUPS: RefCell<Vec<Vec<Retained<NSMenuItem>>>> = const { RefCell::new(Vec::new()) };
}

/// Where a row sits in its group's visible run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Place {
    pub first: bool,
    pub last: bool,
}

/// Each row's place from which rows of one group are hidden (none for a hidden row).
pub(crate) fn places(hidden: &[bool]) -> Vec<Option<Place>> {
    let first = hidden.iter().position(|h| !h);
    let last = hidden.iter().rposition(|h| !h);
    hidden.iter().enumerate().map(|(i, h)| (!h).then_some(Place { first: Some(i) == first, last: Some(i) == last })).collect()
}

/// The open sections, now (`menu::apply_sections`).
pub(crate) fn set(groups: Vec<Vec<Retained<NSMenuItem>>>) {
    GROUPS.with(|g| *g.borrow_mut() = groups);
}

/// `item`'s place in an open section, if it is in one and shows.
pub(crate) fn place(item: &NSMenuItem) -> Option<Place> {
    GROUPS.with(|g| {
        let g = g.borrow();
        let group = g.iter().find(|group| group.iter().any(|i| std::ptr::eq(&**i, item)))?;
        let at = group.iter().position(|i| std::ptr::eq(&**i, item))?;
        let hidden: Vec<bool> = group.iter().map(|i| i.isHidden()).collect();
        places(&hidden)[at]
    })
}

type Layout = unsafe extern "C-unwind" fn(&NSView, Sel);
static LAYOUT: OnceLock<Layout> = OnceLock::new();

/// Wraps the row views' layout once per process: a menu row laid out gets its panel, or loses one it no longer has.
pub(crate) fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(class) = AnyClass::get(c"NSTableRowView") else { return };
        unsafe {
            if let Some(old) = super::submenus_left::replace(class, sel!(layout), std::mem::transmute::<Layout, Imp>(layout)) {
                LAYOUT.set(std::mem::transmute::<Imp, Layout>(old)).ok();
            }
        }
    });
}

unsafe extern "C-unwind" fn layout(this: &NSView, cmd: Sel) {
    if let Some(old) = LAYOUT.get() { unsafe { old(this, cmd) }; }
    paint_row(this);
}

/// A menu's row view (one holding an item view that knows its item): its item.
fn item_of(row: &NSView) -> Option<Retained<NSMenuItem>> {
    let holder = row.subviews().firstObject()?;
    if !objc2_foundation::NSObjectProtocol::respondsToSelector(&*holder, sel!(menuItem)) { return None }
    unsafe { msg_send![&*holder, menuItem] }
}

fn paint_row(row: &NSView) {
    if row.downcast_ref::<NSTableRowView>().is_none() { return }
    let Some(item) = item_of(row) else { return };
    paint(row, place(&item));
}

/// Every row of `window`'s menu again (a section opened or folded, a row of a group shown or hidden).
pub(crate) fn repaint(window: &NSWindow) {
    let Some(table) = super::menu_motion::table_of(window) else { return };
    for v in table.subviews().iter() { paint_row(&v); }
}

/// `repaint` now and once more on the run loop's next turn, in every mode (the menu tracks in its own), after AppKit
/// has laid out the rows a change added. Nothing stays scheduled.
pub(crate) fn repaint_soon(window: &NSWindow) {
    repaint(window);
    let window = objc2::Message::retain(window);
    let block = block2::RcBlock::new(move || repaint(&window));
    let modes = objc2_foundation::NSArray::from_slice(&[unsafe { objc2_foundation::NSRunLoopCommonModes }]);
    unsafe { objc2_foundation::NSRunLoop::currentRunLoop().performInModes_block(&modes, &block) };
}

/// The panel's part on one row: the group's first row rounds its top corners, its last its bottom ones.
fn paint(row: &NSView, place: Option<Place>) {
    let Some(host) = row.layer() else { return };
    // SAFETY: reads the row layer's sublayers on the main thread; AppKit does not mutate them mid-call.
    let existing = unsafe { host.sublayers() }.and_then(|s| s.iter().find(|l| l.name().is_some_and(|n| n.to_string() == NAME)));
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    match (place, existing) {
        (None, Some(layer)) => layer.removeFromSuperlayer(),
        (None, None) => {}
        (Some(p), existing) => {
            let layer = existing.unwrap_or_else(|| {
                let layer = CALayer::new();
                layer.setName(Some(&super::util::ns(NAME)));
                host.insertSublayer_atIndex(&layer, 0);
                layer
            });
            let dark = objc2_app_kit::NSAppearanceCustomization::effectiveAppearance(row).name().to_string().contains("Dark");
            let (white, alpha) = fill(dark);
            let colour = objc2_core_graphics::CGColor::new_generic_gray(white, alpha);
            layer.setBackgroundColor(Some(&colour));
            let b = row.bounds();
            layer.setFrame(CGRect::new(CGPoint::new(INSET, 0.0), CGSize::new((b.size.width - 2.0 * INSET).max(0.0), b.size.height)));
            layer.setCornerRadius(if p.first || p.last { RADIUS } else { 0.0 });
            // The panel's own orientation as drawn (its host's flips included), as `menu_motion`'s bands read theirs.
            layer.setMaskedCorners(corners(p, layer.contentsAreFlipped()));
        }
    }
    CATransaction::commit();
}

/// The corners a row's part rounds, in its host layer's orientation (`flipped`: y grows down, as the table's).
fn corners(p: Place, flipped: bool) -> objc2_quartz_core::CACornerMask {
    use objc2_quartz_core::CACornerMask as C;
    let (low, high) = (C::LayerMinXMinYCorner | C::LayerMaxXMinYCorner, C::LayerMinXMaxYCorner | C::LayerMaxXMaxYCorner);
    let (top, bottom) = if flipped { (low, high) } else { (high, low) };
    let mut out = C::empty();
    if p.first { out |= top; }
    if p.last { out |= bottom; }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_rounds_its_first_and_last_shown_rows() {
        let p = |first, last| Some(Place { first, last });
        // A header and three rows, all shown.
        assert_eq!(places(&[false, false, false, false]), vec![p(true, false), p(false, false), p(false, false), p(false, true)]);
        // The last row hidden (a task off in emergency, the hidden caption line): the one above it ends the group.
        assert_eq!(places(&[false, false, true]), vec![p(true, false), p(false, true), None]);
        // A header alone is both ends.
        assert_eq!(places(&[false]), vec![p(true, true)]);
    }

    #[test]
    fn the_panel_is_gentle_in_both_looks() {
        assert!(fill(true).0 == 1.0 && fill(true).1 > 0.0 && fill(true).1 < 0.12, "a gentle white over the dark material");
        assert!(fill(false).0 == 0.0 && fill(false).1 > 0.0 && fill(false).1 < 0.1, "a soft grey on the near-white light material");
    }

    #[test]
    fn top_corners_follow_the_layer_orientation() {
        use objc2_quartz_core::CACornerMask as C;
        let first = Place { first: true, last: false };
        assert_eq!(corners(first, true), C::LayerMinXMinYCorner | C::LayerMaxXMinYCorner);
        assert_eq!(corners(first, false), C::LayerMinXMaxYCorner | C::LayerMaxXMaxYCorner);
        assert_eq!(corners(Place { first: false, last: false }, true), C::empty());
    }
}
