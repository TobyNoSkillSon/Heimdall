//! Preparing a model end to end, offline: the same sources Splash prepared on this Mac, reached through a temporary
//! Hub cache whose snapshot links point at the real blobs (read-only), into a temporary models root. model.json must
//! equal Splash's but for paths under the temporary roots, the derived files' times, and the metadata key.
mod hub_support;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hub_support::SplashModel;
use serde_json::Value;
use splash::hub::{self, assembly, DraftChoice, Event, PrepareOptions};

/// A temporary Hub cache holding the sources of Splash's record: snapshot links to the real blobs, and refs/main.
fn mirror_cache(splash: &SplashModel, root: &Path) -> PathBuf {
    let cache = root.join("hub");
    for source in splash.record["sources"].as_object().unwrap().values() {
        let (repo, commit) = (source["repo"].as_str().unwrap(), source["revision"].as_str().unwrap());
        let folder = cache.join(hub::cache::folder_name(repo));
        std::fs::create_dir_all(folder.join("refs")).unwrap();
        std::fs::write(folder.join("refs/main"), commit).unwrap();
        for entry in splash.record["files"].as_object().unwrap().values() {
            let path = Path::new(entry["path"].as_str().unwrap());
            let Some(snapshot) = hub::cache::snapshot_of(path) else { continue };
            if snapshot.file_name().unwrap() != commit || !snapshot.starts_with(splash_cache(repo, &snapshot)) {
                continue;
            }
            let link = folder.join("snapshots").join(commit).join(path.strip_prefix(&snapshot).unwrap());
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(path.canonicalize().unwrap(), link).unwrap();
        }
    }
    cache
}

/// The folder of repo in the cache holding `snapshot`.
fn splash_cache(repo: &str, snapshot: &Path) -> PathBuf {
    snapshot.parent().unwrap().parent().unwrap().parent().unwrap().join(hub::cache::folder_name(repo))
}

fn options(root: &Path, cache: PathBuf, engine: Option<PathBuf>) -> (PrepareOptions, Arc<Mutex<Vec<String>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let mut options = PrepareOptions::new(root.join("models"));
    options.offline = true;
    options.hf_cache = Some(cache);
    options.engine = engine;
    options.progress = Some(Arc::new(move |event: &Event| {
        if let Event::Status(line) = event {
            sink.lock().unwrap().push(line.clone());
        }
    }));
    (options, log)
}

/// Compare a model.json with Splash's: every field equal, but for paths under the temporary roots, the derived
/// metadata's file times and key (another adapter release keyed Splash's entry). Returns what differed.
fn compare(ours: &Value, theirs: &Value, models: &Path, cache: &Path) -> Vec<String> {
    let mut differences = Vec::new();
    for key in ["version", "model", "family", "target_format", "vision_format", "sources"] {
        assert_eq!(ours[key], theirs[key], "{key}");
    }
    let (ours_files, theirs_files) = (ours["files"].as_object().unwrap(), theirs["files"].as_object().unwrap());
    assert_eq!(ours_files.keys().collect::<Vec<_>>(), theirs_files.keys().collect::<Vec<_>>());
    for (name, entry) in ours_files {
        let theirs = &theirs_files[name];
        let path = Path::new(entry["path"].as_str().unwrap());
        assert_eq!(entry["digest"], theirs["digest"], "{name} digest");
        assert_eq!(entry["bytes"], theirs["bytes"], "{name} bytes");
        if path.starts_with(models) {
            // Derived here: same content, written now.
            assert!(path.starts_with(models.join(".metadata").join(ours["metadata"].as_str().unwrap())));
            differences.push(format!("{name}: path, mtime_ns, ctime_ns (derived anew)"));
        } else {
            assert!(path.starts_with(cache), "{name} links {}", path.display());
            let relative = path.strip_prefix(cache).unwrap();
            assert!(theirs["path"].as_str().unwrap().ends_with(relative.to_str().unwrap()), "{name} path");
            assert_eq!((&entry["mtime_ns"], &entry["ctime_ns"]), (&theirs["mtime_ns"], &theirs["ctime_ns"]), "{name}");
        }
    }
    if ours["metadata"] != theirs["metadata"] {
        differences.push("metadata key (adapter release)".into());
    }
    differences
}

#[test]
fn offline_prepare_matches_splash_assembly() {
    let Some(splash) = SplashModel::find() else {
        return hub_support::skip("no GGUF model prepared by Splash on this Mac");
    };
    let dir = tempfile::tempdir().unwrap();
    let cache = mirror_cache(&splash, dir.path());
    let engine = hub_support::engine();
    if engine.is_none() {
        hub_support::skip("no Splash engine: the family comes from the Rust rules only");
    }
    let (options, log) = options(dir.path(), cache.clone(), engine);
    let model = splash.record["model"].as_str().unwrap();
    let prepared = hub::prepare(model, &options).unwrap();
    assert_eq!(prepared.family.name, splash.record["family"]);
    let models = options.models_root.canonicalize().unwrap();
    assert_eq!(prepared.root, models.join(model));
    let ours: Value = serde_json::from_slice(&std::fs::read(prepared.root.join("model.json")).unwrap()).unwrap();
    let differences = compare(&ours, &splash.record, &models, &cache);
    eprintln!("model.json differs from Splash's only in: {differences:#?}");
    assert_eq!(ours["metadata"], assembly::metadata_key(ours["files"].as_object().unwrap()));
    // The assembly is laid out as Splash's: the same paths, each a link.
    for name in splash.record["files"].as_object().unwrap().keys() {
        assert!(prepared.assembly.join(name).is_symlink(), "{name}");
    }
    assert_eq!(prepared.assembly, models.join(".resolved").join(hub::cache::sha256_hex(&hub::json::canonical(&ours))));
    assembly::verify(&prepared.root, false).unwrap();
    // Each source snapshot is pinned for this installation, in the temporary cache only.
    let pins = assembly::pins(&ours);
    assert_eq!(pins.len(), 2);
    for (snapshot, _) in &pins {
        assert!(hub::cache::pinned(snapshot, &prepared.root).is_file());
    }
    assert!(log.lock().unwrap().iter().any(|l| l.starts_with("Installing ")));

    // A second start reuses the verified installation.
    log.lock().unwrap().clear();
    let again = hub::prepare(model, &options).unwrap();
    assert_eq!(again.assembly, prepared.assembly);
    assert!(log.lock().unwrap().iter().any(|l| l.contains("is already installed")), "{:?}", log.lock().unwrap());

    // What is present, with byte counts; nothing to fetch.
    let inspection = hub::inspect(model, &options).unwrap();
    assert!(inspection.installed);
    assert_eq!(inspection.missing_bytes, 0);
    let linked: u64 = ["target/Qwen3.8-27B-UD-Q4_K_M.gguf", "vision/mmproj.gguf", "draft/model.safetensors", "draft/config.json"]
        .iter()
        .filter_map(|n| splash.record["files"][n]["bytes"].as_u64())
        .sum();
    if splash.record["files"].get("target/Qwen3.8-27B-UD-Q4_K_M.gguf").is_some() {
        assert_eq!(inspection.present_bytes, linked);
    }

    // A damaged assembly is rebuilt on the next start.
    let link = prepared.assembly.join("draft/config.json");
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink("/nonexistent", &link).unwrap();
    log.lock().unwrap().clear();
    let rebuilt = hub::prepare(model, &options).unwrap();
    assembly::verify(&rebuilt.root, false).unwrap();
    assert!(log.lock().unwrap().iter().any(|l| l.starts_with("Reinstalling") || l.starts_with("Rebuilding")));
}

#[test]
fn language_only_selection_is_its_own_text_assembly() {
    let Some(splash) = SplashModel::find() else {
        return hub_support::skip("no GGUF model prepared by Splash on this Mac");
    };
    let dir = tempfile::tempdir().unwrap();
    let cache = mirror_cache(&splash, dir.path());
    // No engine: the family comes from the Rust rules.
    let (mut options, _log) = options(dir.path(), cache, None);
    options.language_only = true;
    let model = splash.record["model"].as_str().unwrap();
    let prepared = hub::prepare(model, &options).unwrap();
    assert_eq!(prepared.family.name, splash.record["family"]);
    assert!(prepared.root.starts_with(options.models_root.canonicalize().unwrap().join(".selections")));
    let record = assembly::verify(&prepared.root, false).unwrap();
    assert_eq!(record["vision_format"], "none");
    assert!(record["files"].get("vision/mmproj.gguf").is_none());
    // Without the projector, config.json has no vision_config, so the metadata entry is another one.
    assert_ne!(record["metadata"], splash.record["metadata"]);
    let config: Value = serde_json::from_slice(&std::fs::read(prepared.root.join("config.json")).unwrap()).unwrap();
    assert!(config.get("vision_config").is_none());
}

#[test]
fn offline_without_a_cached_commit_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let (options, _log) = options(dir.path(), dir.path().join("empty-cache"), None);
    let error = hub::prepare("unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M", &options).unwrap_err().to_string();
    assert!(error.contains("HF_HUB_OFFLINE is set"), "{error}");
    assert!(error.contains("records a commit"), "{error}");
    let mut local = options.clone();
    local.draft = DraftChoice::Local(dir.path().join("missing"));
    assert!(hub::prepare("o/r", &local).unwrap_err().to_string().contains("local draft directory not found"));
}

#[test]
fn garbage_collection_keeps_only_what_links_or_holds() {
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path();
    for name in ["kept", "held", "stale"] {
        let assembly = models.join(".resolved").join(name);
        std::fs::create_dir_all(&assembly).unwrap();
        std::fs::write(assembly.join("model.json"), "{}").unwrap();
    }
    std::fs::create_dir_all(models.join(".metadata/unused")).unwrap();
    std::fs::create_dir_all(models.join("o")).unwrap();
    assembly::link_selection(&models.join("o/r"), &models.join(".resolved/kept")).unwrap();
    // An owner folder that is itself a link is followed, as glob("*/*") follows it.
    let elsewhere = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(models.join(".resolved/linked-owner")).unwrap();
    std::fs::write(models.join(".resolved/linked-owner/model.json"), "{}").unwrap();
    std::os::unix::fs::symlink(models.join(".resolved/linked-owner"), elsewhere.path().join("r")).unwrap();
    std::os::unix::fs::symlink(elsewhere.path(), models.join("linked")).unwrap();
    let (_, held) = assembly::hold(&models.join(".resolved/held"), &|_| {}).unwrap();
    assembly::collect_garbage(models).unwrap();
    assert!(models.join(".resolved/kept").exists());
    assert!(models.join(".resolved/held").exists());
    assert!(models.join(".resolved/linked-owner").exists());
    assert!(!models.join(".resolved/stale").exists());
    assert!(!models.join(".metadata/unused").exists());
    drop(held);
    // A directory at a selection path is someone's own files: never replaced.
    std::fs::create_dir_all(models.join("o/mine")).unwrap();
    assert!(assembly::link_selection(&models.join("o/mine"), &models.join(".resolved/kept")).is_err());
}

/// A draft whose new commit cannot be downloaded leaves the installed draft in place (upstream._draft), and the
/// failure names the Hub's reason.
#[test]
fn failed_draft_download_keeps_the_installed_draft() {
    let Some(splash) = SplashModel::find() else {
        return hub_support::skip("no GGUF model prepared by Splash on this Mac");
    };
    let dir = tempfile::tempdir().unwrap();
    let cache = mirror_cache(&splash, dir.path());
    let (mut options, log) = options(dir.path(), cache, None);
    let model = splash.record["model"].as_str().unwrap();
    hub::prepare(model, &options).unwrap();

    // The Hub: the target at its installed commit, the draft moved to a commit whose weights fail their digest.
    let target = &splash.record["sources"]["target"];
    let draft = &splash.record["sources"]["draft"];
    let moved = "f".repeat(40);
    let draft_config = std::fs::read(splash.path("draft/config.json")).unwrap();
    let bad_weights = b"not the published weights".to_vec();
    let mut target_files: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, entry) in splash.record["files"].as_object().unwrap() {
        if name.starts_with("target/") || name == "vision/mmproj.gguf" {
            let file = Path::new(entry["path"].as_str().unwrap()).file_name().unwrap().to_string_lossy().into_owned();
            target_files.push((file, Vec::new()));
        }
    }
    let target_listing = serde_json::to_vec(&serde_json::json!({
        "sha": target["revision"],
        "siblings": target_files.iter().map(|(n, _)| serde_json::json!({"rfilename": n})).collect::<Vec<_>>(),
    }))
    .unwrap();
    let draft_listing = hub_support::listing(
        &moved,
        &[("config.json", &draft_config, false), ("model.safetensors", b"the published weights....", true)],
    );
    let mut routes = std::collections::HashMap::new();
    routes.insert(format!("/api/models/{}?blobs=true", target["repo"].as_str().unwrap()), target_listing);
    routes.insert(format!("/api/models/{}?blobs=true", draft["repo"].as_str().unwrap()), draft_listing);
    routes.insert(format!("/{}/resolve/{moved}/config.json", draft["repo"].as_str().unwrap()), draft_config);
    routes.insert(format!("/{}/resolve/{moved}/model.safetensors", draft["repo"].as_str().unwrap()), bad_weights);
    let stub = hub_support::serve(routes);
    // Only this test reads HF_ENDPOINT; the others never go online.
    std::env::set_var("HF_ENDPOINT", &stub.url);
    options.offline = false;
    log.lock().unwrap().clear();
    let prepared = hub::prepare(model, &options).unwrap();
    std::env::remove_var("HF_ENDPOINT");
    let record = assembly::verify(&prepared.root, false).unwrap();
    assert_eq!(record["sources"]["draft"], *draft);
    let lines = log.lock().unwrap().join("\n");
    assert!(lines.contains("keeping the installed one") && lines.contains("does not match its published digest"), "{lines}");
}
