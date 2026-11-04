//! Every floating island hidden or shown at once ("sometimes they are slightly annoying …
//! Control-comma to toggle the floating islands"): ⌃⌘, and Show Islands set `Settings::islands_hidden`,
//! and this writes the helper's `visibility.json` (`{"hidden": true}`). The helper orders its windows out or back in;
//! no island file changes, so showing puts every island back where it was, open or closed. Heimdall's own notch
//! island is not a floating island and stays. While hidden, what needs the user still reaches him through the notch
//! nudge (`nudge.rs`), and a result that arrives meanwhile nudges too.

use super::files::{write_atomic, Root};
use std::sync::atomic::{AtomicBool, Ordering};

static HIDDEN: AtomicBool = AtomicBool::new(false);

/// Whether every floating island is hidden now.
pub fn hidden() -> bool { HIDDEN.load(Ordering::Acquire) }

/// Hide or show every island: the live host's folder gets the flag; without a host (replays, tests) only the
/// in-memory state changes.
pub fn set(hidden: bool) {
    HIDDEN.store(hidden, Ordering::Release);
    if let Some(h) = super::host() {
        if let Err(e) = write(&h.root, hidden) { eprintln!("islands: visibility not written: {}", super::short_error(&e)) }
    }
}

/// The flag file, through a hidden temp file and rename (the helper watches it).
pub fn write(root: &Root, hidden: bool) -> anyhow::Result<()> {
    write_atomic(&root.visibility(), &serde_json::json!({ "hidden": hidden }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_is_the_helper_s_file_and_touches_no_island() {
        let d = tempfile::tempdir().unwrap();
        let root = Root(d.path().join("FloatingIslands"));
        root.put("tests", serde_json::json!({"title": "Tests"}).as_object().unwrap()).unwrap();
        let before = std::fs::read(root.file("tests")).unwrap();
        write(&root, true).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(root.visibility()).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"hidden": true}));
        write(&root, false).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(root.visibility()).unwrap()).unwrap();
        assert_eq!(v, serde_json::json!({"hidden": false}));
        assert_eq!(std::fs::read(root.file("tests")).unwrap(), before, "the island's file is untouched");
        assert_eq!(root.all().len(), 1, "the flag is not an island");
        let names: Vec<String> = std::fs::read_dir(&root.0).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.ends_with(".tmp")), "{names:?}");
    }
}
