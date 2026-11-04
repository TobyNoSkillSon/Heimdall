//! Assemblies: the directories upstream models are served from (install/assembly.py).
//!
//! An assembly is `<models root>/.resolved/<SHA-256 of its record>`: links to source snapshot files and to metadata
//! derived from a GGUF (`<models root>/.metadata/<key>`), and model.json, the record naming its sources and the
//! identity of every linked file. It never changes once published; selection links point to assemblies, and one no
//! selection links and no server holds is collected. Layout and records match Splash 1.4.1 byte for byte, so the
//! engine reads either the same way.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use super::cache::{self, FileLock};
use super::{gguf, is_hex, is_safe_path, json as pyjson, Error, Event, Result};

pub const GGUF_VISION: &str = "vision/mmproj.gguf";
const TARGET_FORMATS: [&str; 2] = ["mlx-affine", "gguf"];
const VISION_FORMATS: [&str; 3] = ["none", "safetensors", "gguf"];
const RECORD_KEYS: [&str; 7] = ["version", "model", "family", "target_format", "vision_format", "sources", "files"];
const FILE_RECORD_KEYS: [&str; 5] = ["path", "bytes", "mtime_ns", "ctime_ns", "digest"];
/// Staging an interrupted installation leaves, which collection removes.
const LINK_STAGING: &str = ".prepare-";
const ENTRY_STAGING: &str = ".loading-";

fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The assembly of `record`, whose files map each assembly path to its source file: an existing one if it verifies,
/// else built again (assembly.build).
pub fn build(models_root: &Path, record: &Value, files: &BTreeMap<String, PathBuf>, progress: &dyn Fn(&Event)) -> Result<PathBuf> {
    let encoded = pyjson::canonical(record);
    let destination = models_root.join(".resolved").join(cache::sha256_hex(&encoded));
    let write = |stage: &Path| -> Result<()> {
        for (name, path) in files {
            let link = stage.join(name);
            fs::create_dir_all(link.parent().expect("in the stage"))?;
            std::os::unix::fs::symlink(absolute(path), &link)?;
        }
        write_durably(&stage.join("model.json"), &encoded)
    };
    reuse_or_write(&destination, write, |d| verify(d, false).map(drop), progress)?;
    Ok(destination)
}

/// Reuse the entry at destination if `check` passes; else write it into a staging folder beside it, published by one
/// rename and checked again. Call under the installation lock.
fn reuse_or_write(
    destination: &Path,
    write: impl FnOnce(&Path) -> Result<()>,
    check: impl Fn(&Path) -> Result<()>,
    progress: &dyn Fn(&Event),
) -> Result<()> {
    if destination.exists() {
        match check(destination) {
            Ok(()) => return Ok(()),
            Err(error) => {
                progress(&Event::Status(format!("Rebuilding the damaged {}: {error}", destination.display())));
                fs::remove_dir_all(destination)?;
            }
        }
    }
    let parent = destination.parent().expect("an entry folder");
    fs::create_dir_all(parent)?;
    let stage = parent.join(format!("{ENTRY_STAGING}{:08x}", rand::random::<u32>()));
    fs::create_dir(&stage)?;
    let written = write(&stage).and_then(|()| fs::rename(&stage, destination).map_err(Error::from));
    if stage.exists() {
        let _ = fs::remove_dir_all(&stage);
    }
    written?;
    check(destination)
}

/// Write a file of an entry, on disk before the entry is published.
fn write_durably(path: &Path, data: &[u8]) -> Result<()> {
    fs::create_dir_all(path.parent().expect("in an entry"))?;
    let mut file = File::create(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

/// What model.json records of a linked file: its path, size and times, which verify compares on every start, and its
/// content digest. `recorded`, an earlier assembly's record of the file, gives the digest without reading the file
/// while the path, size and times still match.
pub fn file_record(path: &Path, recorded: Option<&Value>) -> Result<Value> {
    let stat = fs::metadata(path)?;
    let mut record = Map::new();
    record.insert("path".into(), json!(absolute(path).to_string_lossy()));
    record.insert("bytes".into(), json!(stat.len()));
    // Nanosecond times as i64, as Python's st_mtime_ns: they fit until 2262.
    record.insert("mtime_ns".into(), json!(stat.mtime() * 1_000_000_000 + stat.mtime_nsec()));
    record.insert("ctime_ns".into(), json!(stat.ctime() * 1_000_000_000 + stat.ctime_nsec()));
    let unchanged = recorded.is_some_and(|r| record.iter().all(|(k, v)| r.get(k) == Some(v)));
    let digest = match recorded {
        Some(r) if unchanged => r["digest"].clone(),
        _ => json!(digest(path)?),
    };
    record.insert("digest".into(), digest);
    Ok(Value::Object(record))
}

/// A Hub cache file's name in blobs/ (the digest the Hub publishes for it), else the file's SHA-256.
fn digest(path: &Path) -> Result<String> {
    let blob = path.canonicalize()?;
    let name = blob.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if blob.parent().and_then(Path::file_name).is_some_and(|n| n == "blobs") && (is_hex(&name, 40) || is_hex(&name, 64)) {
        return Ok(name);
    }
    cache::sha256_file(path)
}

/// path's content digest in the scheme of `digest`.
fn digest_of(path: &Path, digest: &str) -> Result<String> {
    if digest.len() == 64 { cache::sha256_file(path) } else { cache::git_sha1_file(path) }
}

fn read_record(path: &Path) -> Result<Value> {
    let data = fs::read(path).map_err(|e| Error::model(format!("could not read {}: {e}", path.display())))?;
    let value: Value =
        serde_json::from_slice(&data).map_err(|e| Error::model(format!("could not read {}: {e}", path.display())))?;
    if !value.is_object() {
        return Err(Error::model(format!("expected a JSON object in {}", path.display())));
    }
    Ok(value)
}

/// The record of an assembly (or a selection link to one) if it verifies: model.json has the shape `build` records
/// and each path still links the file it records with the recorded size and times (with `full`, content). No Hub.
pub fn verify(assembly: &Path, full: bool) -> Result<Value> {
    let record = read_record(&assembly.join("model.json"))?;
    if !well_formed(&record) {
        return Err(Error::model("invalid resolved model record"));
    }
    for (name, entry) in record["files"].as_object().expect("well formed") {
        let path = assembly.join(name);
        let stat = fs::metadata(&path)?;
        let link = fs::read_link(&path).ok();
        if link.as_deref().map(|l| l.to_string_lossy().into_owned()).as_deref() != entry["path"].as_str()
            || json!(stat.len()) != entry["bytes"]
            || json!(stat.mtime() * 1_000_000_000 + stat.mtime_nsec()) != entry["mtime_ns"]
            || json!(stat.ctime() * 1_000_000_000 + stat.ctime_nsec()) != entry["ctime_ns"]
        {
            return Err(Error::model(format!("resolved model file changed: {name}")));
        }
        let recorded = entry["digest"].as_str().expect("well formed");
        if full && digest_of(&path, recorded)? != recorded {
            return Err(Error::model(format!("source content hash mismatch: {name}")));
        }
    }
    Ok(record)
}

fn well_formed(record: &Value) -> bool {
    let Some(map) = record.as_object() else { return false };
    let gguf_target = record["target_format"] == "gguf";
    let mut keys: BTreeSet<&str> = RECORD_KEYS.into_iter().collect();
    if gguf_target {
        keys.insert("metadata");
    }
    let hex = |v: &Value, n| v.as_str().is_some_and(|s| is_hex(s, n));
    let source_ok = |s: &Value| {
        s.as_object().is_some_and(|o| o.len() == 2 && o.contains_key("repo") && o.contains_key("revision"))
            && s["repo"].is_string()
            && (s["revision"].is_null() || hex(&s["revision"], 40))
    };
    let file_ok = |(name, e): (&String, &Value)| {
        is_safe_path(name)
            && e.as_object().is_some_and(|o| o.len() == 5 && FILE_RECORD_KEYS.iter().all(|k| o.contains_key(*k)))
            && e["path"].is_string()
            && ["bytes", "mtime_ns", "ctime_ns"].iter().all(|k| e[*k].is_i64() || e[*k].is_u64())
            && (hex(&e["digest"], 40) || hex(&e["digest"], 64))
    };
    map.keys().map(String::as_str).collect::<BTreeSet<_>>() == keys
        && record["version"].as_i64() == Some(1)
        && !record["version"].is_f64()
        && record["model"].is_string()
        && record["family"].is_string()
        && record["target_format"].as_str().is_some_and(|f| TARGET_FORMATS.contains(&f))
        && record["vision_format"].as_str().is_some_and(|f| VISION_FORMATS.contains(&f))
        && (!gguf_target || hex(&record["metadata"], 64))
        && record["sources"].as_object().is_some_and(|s| {
            s.len() == 2 && s.contains_key("target") && s.contains_key("draft") && s.values().all(source_ok)
        })
        && record["files"].as_object().is_some_and(|f| f.iter().all(file_ok))
}

/// The record of the assembly a selection link names, verified or not; None without one of the shape build writes.
pub fn recorded(link: &Path) -> Option<Value> {
    read_record(&link.join("model.json")).ok().filter(well_formed)
}

/// The Hub snapshot of each source an assembly links files from, with its repository ID: the snapshots its
/// installation pins, found from linked paths (HF_HUB_CACHE may name another folder now).
pub fn pins(record: &Value) -> Vec<(PathBuf, String)> {
    let mut sources = BTreeMap::new();
    for source in record["sources"].as_object().into_iter().flat_map(|s| s.values()) {
        if let (Some(repo), Some(revision)) = (source["repo"].as_str(), source["revision"].as_str()) {
            sources.insert((cache::folder_name(repo), revision.to_string()), repo.to_string());
        }
    }
    let mut snapshots = BTreeMap::new();
    for entry in record["files"].as_object().into_iter().flat_map(|f| f.values()) {
        let Some(path) = entry["path"].as_str().and_then(|p| cache::snapshot_of(Path::new(p))) else { continue };
        let folder = path.parent().and_then(Path::parent).and_then(Path::file_name);
        let key = (folder.map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(), path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        if let Some(repo) = sources.get(&key) {
            snapshots.insert(path, repo.clone());
        }
    }
    snapshots.into_iter().collect()
}

/// The assembly paths of the GGUF files derived metadata comes from, in order: the target's, then the projector.
fn metadata_inputs<T>(files: &BTreeMap<String, T>) -> Vec<String> {
    let mut names: Vec<String> =
        files.keys().filter(|n| n.starts_with("target/") && n.ends_with(".gguf")).cloned().collect();
    if files.contains_key(GGUF_VISION) {
        names.push(GGUF_VISION.into());
    }
    names
}

/// The `.metadata` entry derived from these source records: the same content at another path or time is the same
/// source; the adapter and tokenizers release are part of the derivation (assembly._metadata_key).
fn key_of(sources: &[&Value]) -> String {
    let identity = json!({
        "sources": sources.iter().map(|s| json!({"bytes": s["bytes"], "digest": s["digest"]})).collect::<Vec<_>>(),
        "adapter": gguf::ADAPTER,
        "tokenizers": gguf::TOKENIZERS,
    });
    cache::sha256_hex(&pyjson::canonical(&identity))
}

/// The key of the metadata this port derives from an assembly's GGUF files, given each path's file record.
pub fn metadata_key(file_records: &Map<String, Value>) -> String {
    let records: BTreeMap<String, &Value> = file_records.iter().map(|(k, v)| (k.clone(), v)).collect();
    key_of(&metadata_inputs(&records).iter().map(|n| records[n]).collect::<Vec<_>>())
}

/// The key and the files, by assembly path, of the metadata derived from the GGUF files among `files`: derived once,
/// and again when its entry is damaged. `records` maps each source file to its record. Call under the lock.
pub fn derived_metadata(
    models_root: &Path,
    files: &BTreeMap<String, PathBuf>,
    records: &BTreeMap<PathBuf, Value>,
    progress: &dyn Fn(&Event),
) -> Result<(String, BTreeMap<String, PathBuf>)> {
    let names = metadata_inputs(files);
    let inputs: Vec<&PathBuf> = names.iter().map(|n| &files[n]).collect();
    let sources: Vec<&Value> = inputs.iter().map(|p| &records[*p]).collect();
    let key = key_of(&sources);
    // A split target's metadata is its first file's; the projector comes last.
    let target = inputs.first().ok_or_else(|| Error::model("no GGUF target to derive metadata from"))?;
    let vision = files.get(GGUF_VISION);
    let write = |stage: &Path| -> Result<()> {
        let contents = gguf::derived_files(target, vision.map(PathBuf::as_path))?;
        // A source written while it was read no longer has its record's times.
        for (path, source) in inputs.iter().zip(&sources) {
            if &&file_record(path, Some(source))? != source {
                return Err(Error::model("GGUF source changed while reading metadata"));
            }
        }
        let mut hashes = Map::new();
        for (name, data) in &contents {
            write_durably(&stage.join(name), data)?;
            hashes.insert(name.to_string(), json!(cache::sha256_hex(data)));
        }
        write_durably(&stage.join("files.json"), &pyjson::canonical(&Value::Object(hashes)))
    };
    let entry = models_root.join(".metadata").join(&key);
    reuse_or_write(&entry, write, check_metadata, progress)?;
    let mut derived: BTreeMap<String, PathBuf> =
        gguf::DERIVED_FILES.iter().map(|n| (n.to_string(), entry.join(n))).collect();
    derived.insert("tokenizer/config.json".into(), entry.join("config.json"));
    Ok((key, derived))
}

/// Raise unless a metadata entry holds its files as its files.json lists them.
fn check_metadata(entry: &Path) -> Result<()> {
    let hashes = read_record(&entry.join("files.json"))?;
    let listed: BTreeSet<&str> = hashes.as_object().expect("an object").keys().map(String::as_str).collect();
    if listed != gguf::DERIVED_FILES.into_iter().collect() {
        return Err(Error::model("invalid prepared GGUF metadata record"));
    }
    for name in gguf::DERIVED_FILES {
        if hashes[name].as_str() != Some(cache::sha256_file(&entry.join(name))?.as_str()) {
            return Err(Error::model(format!("prepared GGUF metadata changed: {name}")));
        }
    }
    Ok(())
}

/// Exclusive access to everything written under a models root (models.installation_lock).
pub fn installation_lock(models_root: &Path, progress: &dyn Fn(&Event)) -> Result<FileLock> {
    Ok(FileLock::acquire(&models_root.join(".install.lock"), || {
        progress(&Event::Status("Another Splash model installation is running; waiting...".into()))
    })?)
}

/// Point the selection link at target, atomically. Only a link is ever replaced: a directory there is someone's.
pub fn link_selection(link: &Path, target: &Path) -> Result<()> {
    if link.exists() && !link.is_symlink() {
        return Err(Error::model(format!("refusing to replace non-symlink model path: {}", link.display())));
    }
    let parent = link.parent().expect("under the models root");
    fs::create_dir_all(parent)?;
    let name = link.file_name().expect("a name").to_string_lossy();
    let stage = parent.join(format!("{LINK_STAGING}{name}-{:08x}", rand::random::<u32>()));
    fs::create_dir(&stage)?;
    let temporary = stage.join("model");
    let result = std::os::unix::fs::symlink(target, &temporary).and_then(|()| fs::rename(&temporary, link));
    let _ = fs::remove_file(&temporary);
    let _ = fs::remove_dir(&stage);
    Ok(result?)
}

/// Every selection link under a models root: `*/*` symlinks, hidden folders included (`.selections`).
fn selection_links(models_root: &Path) -> Vec<PathBuf> {
    let mut links = Vec::new();
    for dir in fs::read_dir(models_root).into_iter().flatten().flatten() {
        // As glob("*/*"): an owner folder that is a link to a folder is followed too.
        if dir.path().is_dir() {
            for entry in fs::read_dir(dir.path()).into_iter().flatten().flatten() {
                if entry.path().is_symlink() {
                    links.push(entry.path());
                }
            }
        }
    }
    links
}

/// The assembly a path names and its model.json held open under a shared lock: while it is open, collection keeps
/// the assembly. A server holds the model it serves this way (assembly.hold).
pub fn hold(path: &Path, progress: &dyn Fn(&Event)) -> Result<(PathBuf, File)> {
    let resolved = path.canonicalize().map_err(|e| {
        Error::model(format!("{} names no model Splash prepared: prepare the model again ({e})", path.display()))
    })?;
    let parent = resolved.parent().expect("an assembly");
    if parent.file_name().is_none_or(|n| n != ".resolved") {
        return Err(Error::model(format!("{} is not a model directory Splash prepared", path.display())));
    }
    let models_root = parent.parent().expect("a models root");
    let _lock = installation_lock(models_root, progress)?;
    let assembly = path.canonicalize()?;
    let record = File::open(assembly.join("model.json"))?;
    // SAFETY: flock on the descriptor `record` owns.
    if unsafe { libc::flock(record.as_raw_fd(), libc::LOCK_SH) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok((assembly, record))
}

/// Whether a server holds the assembly.
fn is_held(assembly: &Path) -> bool {
    let Ok(record) = File::open(assembly.join("model.json")) else { return false };
    // SAFETY: as in hold; an exclusive non-blocking try, released when `record` closes.
    unsafe { libc::flock(record.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) != 0 }
}

/// Remove what no installation uses: assemblies no selection links and no server holds, metadata entries the
/// remaining assemblies do not link, and staging an interrupted installation left. Call under the lock.
pub fn collect_garbage(models_root: &Path) -> Result<()> {
    // Links resolve to canonical paths; compare them with canonical entries.
    let models_root = &models_root.canonicalize()?;
    let (resolved, derived) = (models_root.join(".resolved"), models_root.join(".metadata"));
    let linked: BTreeSet<PathBuf> = selection_links(models_root).iter().filter_map(|l| l.canonicalize().ok()).collect();
    let mut used = BTreeSet::new();
    let mut assemblies: Vec<PathBuf> = fs::read_dir(&resolved).into_iter().flatten().flatten().map(|e| e.path()).collect();
    assemblies.sort();
    for assembly in assemblies {
        if !linked.contains(&assembly) && !is_held(&assembly) {
            fs::remove_dir_all(&assembly)?;
            continue;
        }
        let Ok(record) = read_record(&assembly.join("model.json")) else { continue };
        for entry in record["files"].as_object().into_iter().flat_map(|f| f.values()) {
            if let Some(rest) = entry["path"].as_str().and_then(|p| Path::new(p).strip_prefix(&derived).ok()) {
                if let Some(first) = rest.components().next() {
                    used.insert(first.as_os_str().to_os_string());
                }
            }
        }
    }
    for entry in fs::read_dir(&derived).into_iter().flatten().flatten() {
        if !used.contains(&entry.file_name()) {
            fs::remove_dir_all(entry.path())?;
        }
    }
    for dir in fs::read_dir(models_root).into_iter().flatten().flatten() {
        for entry in fs::read_dir(dir.path()).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(LINK_STAGING) && entry.path().is_dir() {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}
