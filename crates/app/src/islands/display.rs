//! Islands Display for the floating islands (bug note 2026-11-07T02-02-35): the helper's `display.json` holds
//! `{"display": "MSI MAG 271QPX"}` while the islands are locked to that display, `{"display": null}` while they
//! follow the pointer (the default). The helper watches the file and lays every island out again on the display it
//! names, or on the pointer's while that display is not connected (`islands::model::DisplayLock`). The UI does the
//! same for the notch island (`ui::islands_display`).

use super::files::{write_atomic, Root};
use std::sync::Mutex;

static LOCKED: Mutex<Option<String>> = Mutex::new(None);

/// The lock's file in the helper's folder.
pub fn path(root: &Root) -> std::path::PathBuf { root.0.join("display.json") }

/// The setting (launch and every apply). It reaches the live host's folder every time (a cheap write, so a failed one
/// is retried by the next apply); without a host (replays, tests) only the in-memory value changes, and `sync`
/// writes it once the host starts.
pub fn set(display: Option<String>) {
    if let Ok(mut l) = LOCKED.lock() { *l = display; }
    sync();
}

/// Writes the current value into the live host's folder (at host start, before the helper launches).
pub fn sync() {
    if let Some(h) = super::host() {
        let display = LOCKED.lock().map(|l| l.clone()).unwrap_or_default();
        if let Err(e) = write(&h.root, display.as_deref()) { eprintln!("islands: display lock not written: {}", super::short_error(&e)) }
    }
}

/// The lock file, through a hidden temp file and rename (the helper watches it).
pub fn write(root: &Root, display: Option<&str>) -> anyhow::Result<()> {
    write_atomic(&path(root), &serde_json::json!({ "display": display }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file the helper reads (`islands::model::DisplayLock`); no island file changes.
    #[test]
    fn the_lock_is_the_helper_s_display_file() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("FloatingIslands"));
        root.put("tests", serde_json::json!({"title": "Tests"}).as_object().unwrap()).unwrap();
        let before = std::fs::read(root.file("tests")).unwrap();
        write(&root, Some("MSI MAG 271QPX")).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "display": "MSI MAG 271QPX" }));
        write(&root, None).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path(&root)).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({ "display": null }), "follow");
        assert_eq!(std::fs::read(root.file("tests")).unwrap(), before);
        assert_eq!(root.all().len(), 1, "the lock is not an island");
    }
}
