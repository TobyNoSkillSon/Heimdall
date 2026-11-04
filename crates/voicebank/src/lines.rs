//! The bank's line list: bundled seed lines, replaced live by an editable `lines.json` in the bank directory.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    /// Stable id (`a-z`, `0-9`, `_`): the decider's option key and the clips' line reference.
    pub id: String,
    pub text: String,
}
/// The decider's "say nothing" option; no line may use it.
pub const NONE: &str = "none";
/// The seed list compiled into Heimdall.
pub const SEED: &str = include_str!("../lines.json");
/// The decider's choice question takes at most 255 options, one of which is `none`.
pub const MAX_LINES: usize = 254;

pub fn parse(bytes: &[u8]) -> Result<Vec<Line>> {
    let lines: Vec<Line> = serde_json::from_slice(bytes).context("voice bank lines are not a JSON list of {id,text}")?;
    ensure!((1..=MAX_LINES).contains(&lines.len()), "voice bank needs 1..{MAX_LINES} lines");
    let mut ids = BTreeSet::new();
    for l in &lines {
        ensure!(
            (1..=64).contains(&l.id.len())
                && l.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
                && l.id != NONE,
            "bad line id {:?}",
            l.id
        );
        ensure!(ids.insert(&l.id), "duplicate line id {}", l.id);
        ensure!(!l.text.trim().is_empty() && l.text.len() <= 200, "line {} text must be 1..200 bytes", l.id);
    }
    Ok(lines)
}
pub fn seed() -> Vec<Line> {
    parse(SEED.as_bytes()).expect("bundled seed lines are valid")
}

/// The lines in use. `current()` re-reads the override file when its modification time changes, so an edit
/// applies to the next decision and fill step without a restart. A missing file means the seed list; an
/// invalid one keeps the seed list and reports why.
/// Override mtime, the lines in use, and why the override was rejected.
type Loaded = (Option<SystemTime>, Arc<Vec<Line>>, Option<String>);
pub struct LineSet {
    path: PathBuf,
    state: Mutex<Loaded>,
}
impl LineSet {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), state: Mutex::new((None, Arc::new(seed()), None)) }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    /// The lines and, when the override file is unusable, the reason.
    pub fn current(&self) -> (Arc<Vec<Line>>, Option<String>) {
        let modified = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
        let mut s = self.state.lock().unwrap();
        if modified != s.0 {
            *s = match modified {
                None => (None, Arc::new(seed()), None),
                Some(at) => match std::fs::read(&self.path).map_err(anyhow::Error::from).and_then(|b| parse(&b)) {
                    Ok(lines) => (Some(at), Arc::new(lines), None),
                    Err(e) => (Some(at), Arc::new(seed()), Some(format!("{}: {e:#}", self.path.display()))),
                },
            };
        }
        (s.1.clone(), s.2.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn seed_is_valid_and_override_applies_live() -> Result<()> {
        let seed = seed();
        assert!(seed.len() >= 50, "about fifty seed lines");
        let t = tempfile::tempdir()?;
        let set = LineSet::new(t.path().join("lines.json"));
        assert_eq!(*set.current().0, seed);
        std::fs::write(set.path(), br#"[{"id":"sure","text":"Sure."}]"#)?;
        assert_eq!(set.current().0[0].id, "sure");
        // Invalid edit: seed list stays usable and the reason is reported.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(set.path(), br#"[{"id":"none","text":"x"}]"#)?;
        let (lines, error) = set.current();
        assert_eq!(*lines, seed);
        assert!(error.unwrap().contains("bad line id"));
        std::fs::remove_file(set.path())?;
        assert_eq!(set.current(), (Arc::new(seed), None));
        assert!(parse(br#"[{"id":"a","text":"x"},{"id":"a","text":"y"}]"#).is_err());
        Ok(())
    }
}
