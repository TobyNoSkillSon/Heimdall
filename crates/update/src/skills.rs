//! Bundled skill updates after an app update, without losing a user's edits (docs/skills.md, "skill upgrades").
//!
//! `crates/agent/src/skills.rs` copies each bundled skill into the profile once. This module runs after it, at each
//! agent start, and keeps a baseline: the hash of the bundled copy last applied to each profile skill
//! (`<store>/skill-baseline.json`). For each bundled skill whose profile copy exists:
//! - profile equals the new bundled copy: nothing to do; the baseline follows it;
//! - profile equals the baseline (unedited since it was shipped) and the bundle changed: the profile copy is replaced
//!   (atomic: staged beside it, renamed in);
//! - otherwise (edited, or no baseline and different): the profile copy is kept byte-identical, the incoming copy is
//!   saved to `<store>/incoming-skills/<name>/`, outside the loaded skill folder, and listed as a conflict.
//!
//! Skills the user made (not bundled) and bundled skills the user removed are never touched.
//!
//! Each release gets one notice in `<store>/inbox/<release>.json` (updated skills, conflicts; an empty notice when
//! nothing changed). The notice is written before any change, never rewritten for the same release, and stays pending
//! until [`acknowledge`], so a crash at any point delivers it exactly once.

use crate::{atomic_write, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const BASELINE: &str = "skill-baseline.json";
pub const INCOMING: &str = "incoming-skills";
pub const INBOX: &str = "inbox";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub release: String,
    pub updated: Vec<String>,
    pub conflicts: Vec<String>,
    #[serde(default)]
    pub delivered: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub updated: Vec<String>,
    pub conflicts: Vec<String>,
    /// True when this call wrote the release's notice (the first start of the release).
    pub notice_written: bool,
}

/// Hash of a skill folder: relative paths and contents, sorted; dotfiles skipped.
pub fn tree_hash(dir: &Path) -> Result<String> {
    let mut files = vec![];
    collect(dir, dir, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for relative in files {
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        let bytes = std::fs::read(dir.join(&relative))?;
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(crate::hex(&hasher.finalize()))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect(root, &path, out)?;
        } else {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    Ok(())
}

/// The bundled skills of `sets` (each a folder of skill folders with SKILL.md), by name; an earlier set wins.
pub fn bundled(sets: &[PathBuf]) -> BTreeMap<String, PathBuf> {
    let mut out = BTreeMap::new();
    for set in sets {
        let Ok(entries) = std::fs::read_dir(set) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.join("SKILL.md").is_file() {
                out.entry(entry.file_name().to_string_lossy().into_owned()).or_insert(path);
            }
        }
    }
    out
}

/// Brings the profile's bundled skills up to this release (see the module comment).
pub fn upgrade(sets: &[PathBuf], profile: &Path, store: &Path, release: &str) -> Result<Report> {
    let baseline_path = store.join(BASELINE);
    let mut baseline: BTreeMap<String, String> =
        std::fs::read(&baseline_path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
    let mut report = Report::default();
    let mut replace = vec![];
    for (name, source) in bundled(sets) {
        let target = profile.join(&name);
        let old = profile.join(format!(".{name}.old"));
        if !target.exists() && old.is_dir() {
            std::fs::rename(&old, &target)?; // a replacement interrupted between its renames
        }
        if !target.join("SKILL.md").is_file() {
            continue; // never seeded, or removed by the user
        }
        let shipped = tree_hash(&source)?;
        let local = tree_hash(&target)?;
        if local == shipped {
            baseline.insert(name, shipped);
        } else if baseline.get(&name) == Some(&local) {
            report.updated.push(name.clone());
            replace.push((name, source, shipped));
        } else {
            report.conflicts.push(name.clone());
            let incoming = store.join(INCOMING).join(&name);
            let part = store.join(INCOMING).join(format!(".{name}.part"));
            let _ = std::fs::remove_dir_all(&part);
            copy_dir(&source, &part)?;
            let _ = std::fs::remove_dir_all(&incoming);
            std::fs::rename(&part, &incoming)?;
        }
    }
    let notice_path = store.join(INBOX).join(format!("{}.json", file_safe(release)));
    if !notice_path.exists() {
        let notice = Notice {
            release: release.to_owned(),
            updated: report.updated.clone(),
            conflicts: report.conflicts.clone(),
            delivered: false,
        };
        atomic_write(&notice_path, &serde_json::to_vec_pretty(&notice).unwrap())?;
        report.notice_written = true;
    }
    for (name, source, shipped) in replace {
        let target = profile.join(&name);
        let part = profile.join(format!(".{name}.part"));
        let old = profile.join(format!(".{name}.old"));
        let _ = std::fs::remove_dir_all(&part);
        let _ = std::fs::remove_dir_all(&old);
        copy_dir(&source, &part)?;
        std::fs::rename(&target, &old)?;
        std::fs::rename(&part, &target)?;
        let _ = std::fs::remove_dir_all(&old);
        baseline.insert(name, shipped);
    }
    atomic_write(&baseline_path, &serde_json::to_vec_pretty(&baseline).unwrap())?;
    Ok(report)
}

/// Notices not yet acknowledged, oldest release first.
pub fn pending(store: &Path) -> Vec<Notice> {
    let mut out: Vec<Notice> = std::fs::read_dir(store.join(INBOX))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|d| serde_json::from_slice::<Notice>(&d).ok())
        .filter(|n| !n.delivered)
        .collect();
    out.sort_by(|a, b| match (crate::Version::parse(&a.release), crate::Version::parse(&b.release)) {
        (Some(x), Some(y)) => x.cmp(&y),
        _ => a.release.cmp(&b.release),
    });
    out
}

/// Marks a release's notice delivered (atomic). Idempotent.
pub fn acknowledge(store: &Path, release: &str) -> Result<()> {
    let path = store.join(INBOX).join(format!("{}.json", file_safe(release)));
    let Ok(data) = std::fs::read(&path) else { return Ok(()) };
    let Ok(mut notice) = serde_json::from_slice::<Notice>(&data) else { return Ok(()) };
    if !notice.delivered {
        notice.delivered = true;
        atomic_write(&path, &serde_json::to_vec_pretty(&notice).unwrap())?;
    }
    Ok(())
}

fn file_safe(release: &str) -> String {
    release.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' }).collect()
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            std::fs::copy(entry.path(), &dest)?;
        }
    }
    Ok(())
}
