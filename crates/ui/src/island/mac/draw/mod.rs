//! Drawing on the screen, both ways (see `crate::drawing`): the user's drawing mode (`overlay`; its keys, Send and light
//! are the island's) and Heimdall's pointing (`point`, faded by the island's frame), with the stroke routine (`ink`),
//! the global keys' Carbon layer (`hotkey`), the system profile (`system`), the agent's app switch (`look`) and the in-process self-check
//! (`check`), and the still it holds over an open menu (`still`).

pub(crate) mod api;
pub(crate) mod check;
pub(crate) mod hotkey;
pub(crate) mod ink;
pub(crate) mod look;
pub(crate) mod overlay;
pub(crate) mod point;
pub(crate) mod still;
pub(crate) mod system;

use std::path::Path;
use std::rc::Rc;

use objc2_foundation::MainThreadMarker;

use crate::drawing::MarkedShot;

/// Drawing mode and pointing at launch: the system profile (measured once per build) and both windows (ordered out,
/// so the capture filters exclude them from the start). ⌃⌘M is `mac/shortcuts.rs`'s, with the other global keys.
pub(crate) fn install(profile: Option<&Path>, sink: Rc<dyn Fn(MarkedShot)>, mtm: MainThreadMarker) {
    if let Some(path) = profile {
        system::install(path, mtm);
    }
    overlay::install(mtm);
    overlay::set_sink(sink);
    point::install(mtm);
}
