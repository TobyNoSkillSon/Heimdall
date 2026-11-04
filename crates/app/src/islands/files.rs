//! The Floating Islands file protocol, Rust side (night 1022). The Swift helper in `islands/` reads and writes the
//! same files: one JSON object per island in `<root>/islands/<id>.json`, the user's clicks in `<root>/responses/`,
//! what is on screen in `<root>/state.json`. Every read-modify-write holds the same `flock` on
//! `islands/.write.lock` as the Swift side, and writes go through a hidden temp file plus rename.
//! Contract: `docs/archive/briefs/night-1022-islands.md`.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The islands data folder: `FLOATING_ISLANDS_DIR` when set (lab runs, tests), else
/// `~/Library/Application Support/FloatingIslands` (shared with the helper and the `islands` CLI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root(pub PathBuf);

impl Root {
    pub fn from_env() -> Root {
        if let Some(d) = std::env::var_os("FLOATING_ISLANDS_DIR").filter(|d| !d.is_empty()) {
            return Root(PathBuf::from(d));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
        Root(home.join("Library/Application Support/FloatingIslands"))
    }
    pub fn islands(&self) -> PathBuf { self.0.join("islands") }
    pub fn responses(&self) -> PathBuf { self.0.join("responses") }
    pub fn docs(&self) -> PathBuf { self.0.join("docs") }
    pub fn state(&self) -> PathBuf { self.0.join("state.json") }
    pub fn pid(&self) -> PathBuf { self.0.join(".app.pid") }
    /// `{"hidden": true}` while every island is off screen (`visibility.rs`).
    pub fn visibility(&self) -> PathBuf { self.0.join("visibility.json") }
    pub fn file(&self, id: &str) -> PathBuf { self.islands().join(format!("{}.json", safe_id(id))) }

    pub fn ensure(&self) -> Result<()> {
        for d in [self.islands(), self.responses()] {
            fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
        }
        Ok(())
    }

    /// Run `body` holding the folder's write lock (the Swift side's `IslandFile.locked`).
    pub fn locked<T>(&self, body: impl FnOnce() -> Result<T>) -> Result<T> {
        self.ensure()?;
        let lock = fs::OpenOptions::new().create(true).read(true).write(true).open(self.islands().join(".write.lock"))?;
        // SAFETY: flock on a file descriptor we own for the duration of the call.
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        let out = body();
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_UN) };
        out
    }

    pub fn read(&self, id: &str) -> Option<Map<String, Value>> { read_object(&self.file(id)) }

    /// Every island on disk, by id, sorted by id.
    pub fn all(&self) -> Vec<(String, Map<String, Value>)> {
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir(self.islands()) else { return out };
        for e in dir.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || p.extension().and_then(|x| x.to_str()) != Some("json") { continue }
            if let (Some(id), Some(o)) = (p.file_stem().and_then(|s| s.to_str()), read_object(&p)) {
                out.push((id.to_string(), o));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Merge `over` into the island (creating it); `null` values remove keys. Returns the written object.
    pub fn put(&self, id: &str, over: &Map<String, Value>) -> Result<Map<String, Value>> {
        self.locked(|| {
            let mut o = self.read(id).unwrap_or_default();
            merge(&mut o, over);
            o.insert("id".into(), Value::String(safe_id(id)));
            write_atomic(&self.file(id), &Value::Object(o.clone()))?;
            Ok(o)
        })
    }

    /// Read-modify-write an existing island; `None` when it does not exist. A change that changes nothing writes
    /// nothing: the thread binding patches every bound island after every T3 pass, and each write wakes the helper.
    pub fn patch(&self, id: &str, change: impl FnOnce(&mut Map<String, Value>)) -> Result<Option<Map<String, Value>>> {
        self.locked(|| {
            let Some(mut o) = self.read(id) else { return Ok(None) };
            let before = o.clone();
            change(&mut o);
            if o != before { write_atomic(&self.file(id), &Value::Object(o.clone()))?; }
            Ok(Some(o))
        })
    }

    pub fn remove(&self, id: &str) -> Result<bool> {
        match fs::remove_file(self.file(id)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// The user's clicks, oldest first, consumed (deleted) as they are read. Malformed files are deleted too.
    pub fn take_responses(&self) -> Vec<Map<String, Value>> {
        let Ok(dir) = fs::read_dir(self.responses()) else { return Vec::new() };
        let mut files: Vec<PathBuf> = dir.flatten().map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json")
                && !p.file_name().and_then(|n| n.to_str()).unwrap_or(".").starts_with('.'))
            .collect();
        // Names are `<island>-<ms>-<pid>-<n>.json`: order by the millisecond stamp, then the name.
        files.sort_by_key(|p| (response_ms(p), p.clone()));
        let mut out = Vec::new();
        for p in files {
            let o = read_object(&p);
            let _ = fs::remove_file(&p);
            if let Some(o) = o { out.push(o) }
        }
        out
    }

    /// What the helper last reported on screen (`state.json`), by island id.
    pub fn screen_state(&self) -> Map<String, Value> { read_object(&self.state()).unwrap_or_default() }
}

/// The Swift side's `Paths.safeID`: letters, digits, `-`, `_`, `.`; anything else becomes `-`.
pub fn safe_id(id: &str) -> String {
    id.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' }).collect()
}

/// Top-level merge; `null` in `over` removes the key (the Swift side's `IslandFile.merge`).
pub fn merge(base: &mut Map<String, Value>, over: &Map<String, Value>) {
    for (k, v) in over {
        if v.is_null() { base.remove(k); } else { base.insert(k.clone(), v.clone()); }
    }
}

pub fn read_object(path: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok()? {
        Value::Object(o) => Some(o),
        _ => None,
    }
}

static WRITES: AtomicU64 = AtomicU64::new(0);

/// Write through a hidden temp file and rename, so the helper's watcher never sees a half-written island.
pub fn write_atomic(path: &Path, value: &Value) -> Result<()> {
    let dir = path.parent().context("island path has no folder")?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).context("island path has no name")?;
    let tmp = dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), WRITES.fetch_add(1, Ordering::Relaxed)));
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path).inspect_err(|_| { let _ = fs::remove_file(&tmp); })?;
    Ok(())
}

fn response_ms(p: &Path) -> u64 {
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // `<island>-<ms>-<pid>-<n>`: the island id may itself contain `-`, so read from the end.
    stem.rsplitn(4, '-').nth(2).and_then(|ms| ms.parse().ok()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root() -> (tempfile::TempDir, Root) {
        let d = tempfile::tempdir().unwrap();
        let r = Root(d.path().to_path_buf());
        (d, r)
    }
    fn obj(v: Value) -> Map<String, Value> { v.as_object().unwrap().clone() }

    #[test]
    fn put_merges_and_null_removes_keys() {
        let (_d, r) = root();
        r.put("tests", &obj(json!({"title": "Tests", "stage": "Building", "side": "left"}))).unwrap();
        let o = r.put("tests", &obj(json!({"stage": "Final tests", "side": null}))).unwrap();
        assert_eq!(o["title"], "Tests");
        assert_eq!(o["stage"], "Final tests");
        assert!(!o.contains_key("side"));
        assert_eq!(r.read("tests").unwrap(), o);
        assert!(fs::read_dir(r.islands()).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn ids_are_made_safe_like_the_swift_side() {
        assert_eq!(safe_id("iPad sale/2"), "iPad-sale-2");
        let (_d, r) = root();
        r.put("a b", &obj(json!({"title": "A"}))).unwrap();
        assert!(r.file("a b").ends_with("islands/a-b.json"));
        assert_eq!(r.all().len(), 1);
    }

    #[test]
    fn patch_skips_missing_islands_and_remove_reports() {
        let (_d, r) = root();
        assert!(r.patch("none", |o| { o.insert("x".into(), json!(1)); }).unwrap().is_none());
        r.put("one", &obj(json!({"title": "One"}))).unwrap();
        assert_eq!(r.patch("one", |o| { o.insert("x".into(), json!(1)); }).unwrap().unwrap()["x"], 1);
        assert!(r.remove("one").unwrap());
        assert!(!r.remove("one").unwrap());
    }

    #[test]
    fn responses_are_consumed_in_time_order_and_bad_ones_dropped() {
        let (_d, r) = root();
        r.ensure().unwrap();
        fs::write(r.responses().join("to-do-200-1-1.json"), r#"{"island":"to-do","kind":"tick","value":"b"}"#).unwrap();
        fs::write(r.responses().join("to-do-100-1-2.json"), r#"{"island":"to-do","kind":"approval","action":"yes","value":"a"}"#).unwrap();
        fs::write(r.responses().join("x-150-1-3.json"), "not json").unwrap();
        let got = r.take_responses();
        assert_eq!(got.iter().map(|o| o["value"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(r.take_responses().is_empty());
        assert_eq!(fs::read_dir(r.responses()).unwrap().count(), 0);
    }

    #[test]
    fn a_patch_that_changes_nothing_does_not_rewrite_the_file() {
        let d = tempfile::tempdir().unwrap();
        let r = Root(d.path().to_path_buf());
        r.put("a", serde_json::json!({"title": "A"}).as_object().unwrap()).unwrap();
        let ino = |r: &Root| std::os::unix::fs::MetadataExt::ino(&fs::metadata(r.file("a")).unwrap());
        let before = ino(&r);
        assert_eq!(r.patch("a", |_| {}).unwrap().unwrap()["title"], "A");
        assert_eq!(ino(&r), before, "no write (a write replaces the file by rename)");
        r.patch("a", |o| { o.insert("title".into(), serde_json::json!("B")); }).unwrap();
        assert_ne!(ino(&r), before);
        assert_eq!(r.read("a").unwrap()["title"], "B");
    }
}
