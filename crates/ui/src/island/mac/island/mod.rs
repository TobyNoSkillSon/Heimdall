//! The island's AppKit side: `object::Island` painted by one layer tree (`paint`) in one window at the notch, its
//! click target and its clock (`host`), text measured with AppKit (`measure`), content drawn once per state
//! (`raster`), and the offscreen renders and checks (`checks`). Module paths stay `crate::mac::island::*`.
//!
//! The rest of the UI talks to the island only through `apply` (and `notice` for lines with a pull-down, whose body
//! it measures first); the island talks back through the sink (`UiAction`s) and the health handler.

pub(crate) mod bench;
pub(crate) mod checks;
pub(crate) mod copy;
pub(crate) mod field;
pub(crate) mod host;
pub(crate) mod loops;
pub(crate) mod measure;
pub(crate) mod paint;
pub(crate) mod raster;

use objc2_foundation::MainThreadMarker;

pub(crate) use self::field::digit_pressed;
use self::host::{with_host, Host, Place, HOST};
use crate::agent_notice::{AgentNotice, Detail, NoticeKind};
use crate::island::object::Event;
use crate::island::{CommandCue, HealthControl, IslandReport};
use crate::model::UiAction;

/// Note drawings kept (the newest few): more than one turn holds (`MAX_TURN_NOTES`).
const MAX_NOTES: usize = crate::island::object::MAX_TURN_NOTES + 2;

/// Creates the island at launch (windows ordered out, so the capture filters can exclude them from the start).
pub(crate) fn install(mtm: MainThreadMarker) {
    if HOST.with(|h| h.borrow().is_some()) {
        return;
    }
    let place = super::util::pointer_screen(mtm).map(|s| Place::of(&s)).unwrap_or_else(Place::offscreen);
    let host = Host::new(mtm, place, false);
    HOST.with(|h| *h.borrow_mut() = Some(host));
}

/// Creates the island for checks and renders: never on a display, on a virtual clock starting at 0.
pub(crate) fn install_offscreen(mtm: MainThreadMarker) {
    let mut host = Host::new(mtm, Place::offscreen(), true);
    host.clock = Some(0.0);
    HOST.with(|h| *h.borrow_mut() = Some(host));
}

/// Where the island's actions go (crates/app, as `UiAction`s).
pub(crate) fn set_sink(sink: Box<dyn Fn(UiAction)>) {
    host::set_sink(sink);
}

/// Who opens Settings or the menu for a problem's fix button (the UI state), and tells the app.
pub(crate) fn set_health_handler(h: Box<dyn Fn(String, HealthControl)>) {
    host::set_health(h);
}

/// Anything that happens to the island.
pub(crate) fn apply(ev: Event) {
    with_host(|h| h.apply(ev));
}

/// An agent notice. A line with a pull-down has its body drawn first (as wide as its content needs, up to what the
/// display allows), so the island knows its size.
pub(crate) fn notice(notice: AgentNotice) {
    with_host(|h| {
        let body = notice.detail.as_ref().and_then(|d| {
            let log = matches!(d, Detail::Text(_)) && notice.kind == NoticeKind::Error;
            let doc = raster::note(d, log, crate::island::object::note_max_w(&h.place.display), h.place.display.scale)?;
            let size = (doc.w, doc.h);
            h.tree.rasters.keep_note(&notice.id, doc, MAX_NOTES);
            Some(size)
        });
        h.apply(Event::Notice { notice, body });
    });
}

/// ⌃⌘H: the conversation pulled down with its composer focused (again, while the user types in it, pulls it up).
pub(crate) fn open_chat() {
    with_host(|h| h.open_chat());
}

/// Something needs the user: the menu-bar helmet is red.
pub(crate) fn alerting() -> bool {
    with_host(|h| h.island.alerting()).unwrap_or(false)
}

/// The active Space changed: a showing island is ordered in again there.
pub(crate) fn space_changed() {
    with_host(|h| {
        let now = h.now();
        if h.island.frame(now).visible && !h.offscreen {
            let (panel, control) = h.panels();
            panel.orderFrontRegardless();
            h.join_overlay(&panel, crate::island::ISLAND_WINDOW_TITLE);
            if control.isVisible() {
                control.orderFrontRegardless();
                h.join_overlay(&control, host::CONTROL_WINDOW_TITLE);
            }
        }
    });
}

/// A command cue the island never received (Listening off): reported, not shown.
pub(crate) fn not_taken(cue: &CommandCue, why: &'static str) {
    if let Some(r) = IslandReport::not_taken(cue, why) {
        host::emit(UiAction::Island(r));
    }
}

/// Drawing mode keeps the island on its display, and above its overlay (which sits above menus) while it is up.
/// Pointing rises with them. A busy host is pinned on the next turn of the main queue, so the windows never part.
pub(crate) fn pin(display: Option<u32>) {
    let done = with_host(|h| {
        h.pinned = display;
        let (island, control) = host::levels(display.is_some());
        let (panel, target) = h.panels();
        panel.setLevel(island);
        target.setLevel(control);
        if let Some(pointer) = super::draw::point::panel() {
            pointer.setLevel(if display.is_some() { super::draw::overlay::LEVEL } else { super::draw::point::LEVEL });
        }
    });
    let busy = done.is_none() && HOST.with(|h| h.try_borrow().is_err());
    if busy {
        dispatch2::DispatchQueue::main().exec_async(move || pin(display));
    }
}
