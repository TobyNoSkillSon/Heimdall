//! Show Islands on Streams for the floating islands ("hide all islands"): the helper's
//! `streams.json` holds `{"shown": true}` while its islands appear in screen shares, streams and screenshots. Missing
//! or false: every floating island window is unshared (`NSWindowSharingType::None`), the default. The helper watches
//! the file and applies it to every island window at once; the UI does the same for the notch island
//! (`ui::…::streams`). Unshared windows are out of every capture, Heimdall's own screenshots included: macOS has no
//! per-app exception.

use super::files::{write_atomic, Root};
use std::sync::atomic::{AtomicBool, Ordering};

static SHOWN: AtomicBool = AtomicBool::new(false);

/// The flag's file in the helper's folder.
pub fn path(root: &Root) -> std::path::PathBuf { root.0.join("streams.json") }

/// The switch from settings (launch and every apply). It reaches the live host's folder every time (a cheap write,
/// so a failed one is retried by the next apply); without a host (replays, tests) only the in-memory value changes,
/// and `sync` writes it once the host starts.
pub fn set(shown: bool) {
    SHOWN.store(shown, Ordering::Release);
    sync();
}

/// Writes the current value into the live host's folder (at host start, before the helper launches).
pub fn sync() {
    if let Some(h) = super::host() {
        if let Err(e) = write(&h.root, SHOWN.load(Ordering::Acquire)) { eprintln!("islands: streams switch not written: {}", super::short_error(&e)) }
    }
}

/// The flag file, through a hidden temp file and rename (the helper watches it).
pub fn write(root: &Root, shown: bool) -> anyhow::Result<()> {
    write_atomic(&path(root), &serde_json::json!({ "shown": shown }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file the helper reads (`islands::model::Streams`); no island file changes.
    #[test]
    fn the_switch_is_the_helper_s_streams_file() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("FloatingIslands"));
        root.put("tests", serde_json::json!({"title": "Tests"}).as_object().unwrap()).unwrap();
        let before = std::fs::read(root.file("tests")).unwrap();
        for shown in [true, false] {
            write(&root, shown).unwrap();
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path(&root)).unwrap()).unwrap();
            assert_eq!(v, serde_json::json!({ "shown": shown }), "the helper's format (crates/islands tests/model.rs)");
        }
        assert_eq!(std::fs::read(root.file("tests")).unwrap(), before);
    }
}
