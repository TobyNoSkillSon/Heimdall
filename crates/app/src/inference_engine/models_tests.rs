//! The engine's catalog and its models on disk: every test works in a temp dir with fixture installations and a fake
//! installer script; nothing touches the real Hugging Face cache or starts `heimdall-splash`.
use super::models::{self, fixture, DownloadEvent, Installer, Roots};
use super::*;
use std::os::unix::fs::{symlink, PermissionsExt};
use tempfile::TempDir;

const DRAFT: &str = "incoai/Qwen3.8-27B-DFlash2";
const Q4: &str = "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q4_K_XL";
const Q8: &str = "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:Q8_0";
const BONSAI: &str = "OS-Software/Ternary-Bonsai-2-27B-Uncensored-Heretic-v2-GGUF:PQ2_0";

fn roots() -> (TempDir, Roots) {
    let t = TempDir::new().unwrap();
    let roots = Roots { models: t.path().join("data/splash/models"), hf: t.path().join("hf/hub"), others: vec![t.path().join("splash-homebrew/models")], manifests: t.path().join("data/splash/downloads") };
    (t, roots)
}
/// Every file under a folder (links included, not followed), relative.
fn files(root: &Path) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for e in fs::read_dir(dir).into_iter().flatten().flatten() {
            let t = e.file_type().unwrap();
            if t.is_dir() { walk(root, &e.path(), out) } else { out.insert(e.path().strip_prefix(root).unwrap().display().to_string()); }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}
fn blob_of(roots: &Roots, repo: &str, name: &str) -> PathBuf {
    roots.hf.join(format!("models--{}", repo.replace('/', "--"))).join("snapshots").join(fixture::commit(repo)).join(name).canonicalize().unwrap()
}
use std::collections::BTreeSet;

/// "i want all of these options in the splash engine picker" — six models, 24 precisions, each from
/// its uncensored repository with its download size and derived memory; the ones over the 48 GB ceiling are listed.
#[test]
fn the_catalog_lists_six_models_and_24_precisions_with_sizes_and_memory() {
    let config = Config::bundled();
    let ids: Vec<_> = config.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["qwen3.8-27b", "qwen3.6-35b-a3b", "flint", "ternary-bonsai-2-27b", "ornith-1.5-122b", "qwen3.8-flash-next"]);
    assert_eq!(config.models.iter().map(|m| m.quants.len()).sum::<usize>(), 24);
    let catalog: Value = serde_json::from_str(include_str!("../../config/inference-engine.json")).unwrap();
    assert!(catalog["ram_formula"].as_str().unwrap().contains("derived"));
    for m in &config.models {
        assert_eq!(m.context, Some(262_144), "{}", m.id);
        assert!(m.quant(&m.quant).is_some() && m.source == m.quant(&m.quant).unwrap().source, "{} default", m.id);
        let mut last = 0;
        for q in &m.quants {
            check_source(&q.source).unwrap();
            assert!(q.source.ends_with(&format!(":{}", q.quant)), "{}", q.source);
            assert!(!q.source.starts_with("unsloth/"), "the uncensored repositories only: {}", q.source);
            let (size, ram) = (q.size_bytes.unwrap(), q.ram_bytes.unwrap());
            assert!(size >= last && ram > size + 1_000_000_000, "{} smallest first, memory over weights", q.source);
            last = size;
        }
    }
    // The 27B keeps its id and moves to the uncensored repository's default; the old unsloth precision is not offered.
    let qwen = config.model("qwen3.8-27b").unwrap();
    assert_eq!((qwen.quant.as_str(), qwen.source.as_str()), ("UD-Q4_K_XL", Q4));
    assert!(qwen.quants.iter().all(|q| q.quant != "UD-Q4_K_M"));
    assert_eq!(qwen.chosen(Some("UD-Q4_K_M")).quant, "UD-Q4_K_XL", "an old choice serves the default");
    assert_eq!(qwen.quant("UD-Q2_K_XL").unwrap().ram_bytes, Some(24_382_102_508), "weights + projector + draft + KV + 1 GB");
    // Over the ceiling: listed with their memory, the menu warns.
    let limit = config.memory_limit_bytes().unwrap();
    assert_eq!(limit, 51_539_607_552);
    let over: Vec<_> = config.models.iter().flat_map(|m| m.quants.iter()).filter(|q| q.ram_bytes.unwrap() > limit).map(|q| q.quant.as_str()).collect();
    assert_eq!(over, ["Q4_K_M-UNCENSORED", "IQ2_XS", "IQ3_XXS", "IQ3_S"]);
    let ui::UiUpdate::Engine { models, memory_limit_bytes, .. } = config.update("qwen3.8-27b", ui::EngineState::Off, None, None) else { panic!() };
    assert_eq!(memory_limit_bytes, Some(limit));
    assert_eq!(models.iter().map(|m| m.quants.len()).sum::<usize>(), 24);
    let ornith = models.iter().find(|m| m.id == "ornith-1.5-122b").unwrap();
    assert_eq!(ornith.quants.last().unwrap().ram_bytes, Some(86_362_554_333));
    assert!(models.iter().all(|m| m.quants.iter().all(|q| !q.downloaded)));
}

/// An override in the older shape (one `source` per model) still loads as a one-precision model; a broken one falls
/// back to the bundled catalog with a note; the ceilings still hold.
#[test]
fn an_old_shape_override_loads_and_a_bad_source_falls_back() {
    let t = TempDir::new().unwrap();
    fs::create_dir_all(t.path().join("splash")).unwrap();
    fs::write(t.path().join("splash/engine.json"), br#"{"executable":"heimdall-splash","port":8100,"max_memory":"48G","max_cache_disk":"100G","default_model":"qwen3.8-27b","models":[{"id":"qwen3.8-27b","label":"Qwen3.8 27B","quant":"UD-Q4_K_M","source":"unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M"}]}"#).unwrap();
    let config = Config::load(t.path()).unwrap();
    let m = config.model("qwen3.8-27b").unwrap();
    assert_eq!(m.quants.len(), 1);
    assert_eq!(m.chosen(None).source, "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M");
    assert_eq!(config.args("qwen3.8-27b", "UD-Q4_K_M", Path::new("/c")).unwrap()[2], "unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M");
    // A source that would name a path outside the models root is refused.
    fs::write(t.path().join("splash/engine.json"), br#"{"executable":"heimdall-splash","port":8100,"max_memory":"48G","max_cache_disk":"100G","default_model":"m","models":[{"id":"m","label":"M","quant":"q","source":"../../etc:q"}]}"#).unwrap();
    let (config, note) = Config::effective(t.path());
    assert!(note.is_some_and(|n| n.contains("bundled default")));
    assert_eq!(config.models.len(), 6);
    for bad in ["a", "a/b/c", "../x:q", "a/b:../c", "a--b/c:q", "/a/b:q", "a/b:"] {
        assert!(check_source(bad).is_err(), "{bad}");
    }
}

/// Downloaded means the selection link resolves to an assembly whose listed files are all there at their size.
#[test]
fn downloaded_detection_reads_the_selection_link_and_its_files() {
    let (_t, roots) = roots();
    assert!(!models::installed(&roots.models, Q4), "nothing yet");
    let assembly = fixture::install_quant(&roots, Q4, DRAFT);
    assert!(models::installed(&roots.models, Q4));
    assert!(!models::installed(&roots.models, Q8), "another precision of the same repository is not");
    // A missing blob (a pruned cache) or a truncated one is not downloaded.
    let blob = blob_of(&roots, DRAFT, "model.safetensors");
    let bytes = fs::read(&blob).unwrap();
    fs::write(&blob, b"x").unwrap();
    assert!(!models::installed(&roots.models, Q4), "a size change");
    fs::remove_file(&blob).unwrap();
    assert!(!models::installed(&roots.models, Q4), "a missing file");
    fs::write(&blob, bytes).unwrap();
    assert!(models::installed(&roots.models, Q4));
    // A link to something that is not an assembly is not.
    fs::remove_file(roots.models.join(Q4)).unwrap();
    symlink(assembly.parent().unwrap(), roots.models.join(Q4)).unwrap();
    assert!(!models::installed(&roots.models, Q4));
}

/// Delete removes that precision's link, assembly, metadata, pins and the cache files only it uses; the projector
/// and draft another installed precision links stay, and so do blobs another snapshot or an unknown pin holds.
#[test]
fn delete_removes_only_that_precisions_files_and_keeps_shared_blobs() {
    let (_t, roots) = roots();
    let q4 = fixture::install_quant(&roots, Q4, DRAFT);
    fixture::install_quant(&roots, Q8, DRAFT);
    // Bonsai shares the 27B's draft repository.
    fixture::install_quant(&roots, BONSAI, DRAFT);
    let hf = roots.hf.canonicalize().unwrap();
    let before = files(&hf);
    let q4_gguf = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf");
    let projector = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "mmproj.gguf");
    let draft = blob_of(&roots, DRAFT, "model.safetensors");
    let owner = models::pin_owner(&roots.models.canonicalize().unwrap().join(Q4));

    let deleted = models::delete(&roots, Q4).unwrap();
    assert!(!roots.models.join(Q4).exists() && !q4.exists(), "its link and assembly");
    assert!(!q4_gguf.exists(), "its weights");
    assert!(projector.exists() && draft.exists(), "the projector Q8 links and the draft Bonsai links stay");
    assert!(models::installed(&roots.models, Q8) && models::installed(&roots.models, BONSAI));
    assert!(deleted.bytes >= "weights of ".len() as u64);
    let gone: Vec<_> = before.difference(&files(&hf)).cloned().collect();
    assert!(gone.iter().all(|f| f.contains("UD-Q4_K_XL") || f.contains(&owner) || f.starts_with("models--huihui-ai--Huihui-Qwen3.8-27B-abliterated-GGUF/blobs/")), "{gone:?}");
    assert!(gone.iter().any(|f| f.contains(&format!("refs/splash/{owner}/"))), "its pins go: {gone:?}");
    // Deleting Q8 now takes the projector too (nothing else links it); the draft stays for Bonsai.
    models::delete(&roots, Q8).unwrap();
    assert!(!projector.exists() && draft.exists());
    // A blob another snapshot entry links stays (a second commit of the draft repository).
    let other = hf.join(format!("models--{}", DRAFT.replace('/', "--"))).join("snapshots").join("f".repeat(40));
    fs::create_dir_all(&other).unwrap();
    symlink(&draft, other.join("model.safetensors")).unwrap();
    models::delete(&roots, BONSAI).unwrap();
    assert!(draft.exists(), "another snapshot links it");
    assert!(!blob_of_exists(&roots, DRAFT, "config.json"), "the draft's config only Bonsai used goes");
    assert!(models::delete(&roots, Q4).is_err(), "already gone");
}
fn blob_of_exists(roots: &Roots, repo: &str, name: &str) -> bool {
    roots.hf.join(format!("models--{}", repo.replace('/', "--"))).join("snapshots").join(fixture::commit(repo)).join(name).exists()
}

/// A snapshot an installation outside these roots pins (refs/splash of a link this Mac's roots do not show) keeps its
/// files; another root's installation (Homebrew Splash) keeps the files it links.
#[test]
fn delete_keeps_what_unknown_pins_and_other_roots_use() {
    let (_t, roots) = roots();
    fixture::install_quant(&roots, Q4, DRAFT);
    let elsewhere = Roots { models: roots.others[0].clone(), hf: roots.hf.clone(), others: vec![], manifests: roots.manifests.join("elsewhere") };
    fixture::install_quant_as(&elsewhere, Q8, DRAFT, false);
    let gguf = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf");
    let draft = blob_of(&roots, DRAFT, "model.safetensors");
    // An unknown installation pins the target snapshot.
    let repo = roots.hf.join("models--huihui-ai--Huihui-Qwen3.8-27B-abliterated-GGUF");
    let pin = repo.join("refs/splash").join("0".repeat(64)).join(fixture::commit("huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF"));
    fs::create_dir_all(pin.parent().unwrap()).unwrap();
    fs::write(&pin, "x").unwrap();
    models::delete(&roots, Q4).unwrap();
    assert!(gguf.exists(), "pinned by an installation these roots do not show");
    assert!(draft.exists(), "the other root's installation links it");
    assert!(!roots.models.join(Q4).exists());
    assert!(models::installed(&elsewhere.models, Q8));
}

/// Paths outside the roots, or reached through a link out of them, are never removed; a held assembly (a running
/// server's shared flock) and a non-link selection path are refused.
#[test]
fn delete_refuses_escapes_held_assemblies_and_plain_folders() {
    let (t, roots) = roots();
    let assembly = fixture::install_quant(&roots, Q4, DRAFT);
    // The engine holds it.
    let held = File::open(assembly.join("model.json")).unwrap();
    assert_eq!(unsafe { libc::flock(held.as_raw_fd(), libc::LOCK_SH) }, 0);
    let e = models::delete(&roots, Q4).unwrap_err();
    assert!(format!("{e:#}").contains("using it"), "{e:#}");
    assert!(models::installed(&roots.models, Q4), "nothing removed");
    drop(held);
    // A record naming files outside the hub cache (or through a link out of it) keeps them.
    let outside = t.path().join("precious");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("keep.gguf"), b"keep").unwrap();
    let escape = roots.hf.join("models--evil--repo/snapshots").join("e".repeat(40));
    fs::create_dir_all(escape.parent().unwrap()).unwrap();
    symlink(&outside, &escape).unwrap();
    let mut record: Value = serde_json::from_slice(&fs::read(assembly.join("model.json")).unwrap()).unwrap();
    record["files"]["target/outside.gguf"] = serde_json::json!({"path": outside.join("keep.gguf").display().to_string(), "bytes": 4});
    record["files"]["target/escape.gguf"] = serde_json::json!({"path": escape.join("keep.gguf").display().to_string(), "bytes": 4});
    fs::write(assembly.join("model.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    models::delete(&roots, Q4).unwrap();
    assert_eq!(fs::read(outside.join("keep.gguf")).unwrap(), b"keep");
    // A selection path that is a folder, an owner folder that links elsewhere, or a bad source is refused.
    let plain = roots.models.join(Q8);
    fs::create_dir_all(&plain).unwrap();
    fs::write(plain.join("model.json"), b"{}").unwrap();
    assert!(models::delete(&roots, Q8).is_err() && plain.join("model.json").exists());
    let owner = roots.models.join("OS-Software");
    symlink(&outside, &owner).unwrap();
    fs::create_dir_all(outside.join("x")).unwrap();
    symlink(outside.join("x"), outside.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-v2-GGUF:PQ2_0")).unwrap();
    assert!(models::delete(&roots, BONSAI).is_err());
    assert!(outside.join("x").exists() && outside.join("Ternary-Bonsai-2-27B-Uncensored-Heretic-v2-GGUF:PQ2_0").is_symlink());
    assert!(models::delete(&roots, "../../precious:keep").is_err());
}

#[test]
fn progress_lines_parse_as_splash_prints_them() {
    let line = "18:02:11 Downloading · huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF · Huihui-Qwen3.8-27B-abliterated-UD-Q4_K_XL.gguf · 45% of 17.4 GB";
    let (repo, file, done, total) = models::parse_progress(line).unwrap();
    assert_eq!((repo.as_str(), file.as_str()), ("huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "Huihui-Qwen3.8-27B-abliterated-UD-Q4_K_XL.gguf"));
    assert_eq!((done, total), (7_830_000_000, 17_400_000_000));
    for other in ["18:02:11 Fetching 2 file(s), 18.31 GB, from x@y; cached files are reused.", "Downloading · a · b", "Downloading · a · b · lots"] {
        assert!(models::parse_progress(other).is_none(), "{other}");
    }
}

/// A fake `heimdall-splash`: records its arguments, environment and priority, prints progress, then links the
/// prepared assembly (`ASSEMBLY`) or fails, or hangs with a child for Cancel.
pub(crate) fn fake_installer(dir: &Path, mode: &str, assembly: Option<&Path>) -> PathBuf {
    let script = dir.join("heimdall-splash");
    let assembly = assembly.map(|a| a.display().to_string()).unwrap_or_default();
    fs::write(&script, format!(r#"#!/bin/sh
echo "$@" > "{dir}/args"; echo "$HF_HUB_CACHE" >> "{dir}/args"; ps -o nice= -p $$ >> "{dir}/args"; echo $$ > "{dir}/pid"
repo="${{2%%:*}}"
echo "18:00:00 Fetching 1 file(s), 2.00 GB, from $repo@abc; cached files are reused." >&2
for p in 0 5 10 15 20 25 30 35 40 45 50 55 60 65 70 75 80 85 90 95 100; do
  echo "18:00:01 Downloading · $repo · w.gguf · $p% of 2.0 GB" >&2
done
case "{mode}" in
  fail) echo "18:00:02 Error: the Hub refused the request (401)" >&2; exit 1 ;;
  linger) sleep 30 & echo $! > "{dir}/child"; mkdir -p "$(dirname "$4/$2")"; ln -s "{assembly}" "$4/$2"; exit 0 ;;
  hang) mkdir -p "$HF_HUB_CACHE/models--$(echo "$repo" | sed 's#/#--#')/blobs"; echo partial > "$HF_HUB_CACHE/models--$(echo "$repo" | sed 's#/#--#')/blobs/abc.incomplete"
        sleep 300 & echo $! > "{dir}/child"; wait ;;
  *) mkdir -p "$(dirname "$4/$2")"; ln -s "{assembly}" "$4/$2"; echo "$4/$2" ;;
esac
"#, dir = dir.display())).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    script
}
fn events_until_done(rx: &crossbeam_channel::Receiver<DownloadEvent>) -> Vec<DownloadEvent> {
    let mut out = vec![];
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(e) = rx.recv_timeout(Duration::from_millis(100)) {
            let done = matches!(e, DownloadEvent::Done { .. });
            out.push(e);
            if done { return out; }
        }
    }
    panic!("no Done: {out:?}");
}
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// The installer runs `pull <source> --models-dir <models root>` with the hub cache in HF_HUB_CACHE at nice 19;
/// progress is throttled to four updates a second; a failure carries Splash's last words.
#[test]
fn a_download_runs_pull_at_low_priority_with_throttled_progress() {
    let (t, roots) = roots();
    // The installation the fake "downloads": built, then its link removed for the fake to make.
    let assembly = fixture::install_quant(&roots, Q4, DRAFT);
    fs::remove_file(roots.models.join(Q4)).unwrap();
    let installer = Installer::Program(fake_installer(t.path(), "ok", Some(&assembly)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let started = Instant::now();
    let d = models::Download::start(&installer, &roots, "qwen3.8-27b", "UD-Q4_K_XL", Q4, Some(17_378_626_464), tx).unwrap();
    let events = events_until_done(&rx);
    let progress = events.iter().filter(|e| matches!(e, DownloadEvent::Progress { .. })).count();
    // The start, then at most one per 250 ms, then the last value before Done.
    assert!(progress as u128 <= 3 + started.elapsed().as_millis() / 250, "throttled: {progress} in {:?}", started.elapsed());
    assert!(matches!(events.first(), Some(DownloadEvent::Progress { done: 0, total: Some(17_378_626_464), .. })));
    assert_eq!(events.last(), Some(&DownloadEvent::Done { id: "qwen3.8-27b".into(), quant: "UD-Q4_K_XL".into(), result: Ok(()), canceled: false }));
    drop(d);
    let args = fs::read_to_string(t.path().join("args")).unwrap();
    let lines: Vec<_> = args.lines().collect();
    assert_eq!(lines[0], format!("pull {Q4} --models-dir {}", roots.models.display()));
    assert_eq!(lines[1], roots.hf.display().to_string());
    // 19 from a normal shell; 20 (the ceiling) when the test itself runs under `nice -n 19`, as the night check does.
    assert!(matches!(lines[2].trim(), "19" | "20"), "lowest priority: {}", lines[2].trim());
    assert!(models::installed(&roots.models, Q4));
    // A failure: the reason without its time or "Error: ".
    let installer = Installer::Program(fake_installer(t.path(), "fail", None));
    let (tx, rx) = crossbeam_channel::unbounded();
    let _d = models::Download::start(&installer, &roots, "qwen3.8-27b", "Q8_0", Q8, None, tx).unwrap();
    let events = events_until_done(&rx);
    assert!(events.iter().any(|e| matches!(e, DownloadEvent::Progress { done, total: Some(2_000_000_000), .. } if *done > 0)), "{events:?}");
    assert_eq!(events.last(), Some(&DownloadEvent::Done { id: "qwen3.8-27b".into(), quant: "Q8_0".into(), result: Err("the Hub refused the request (401)".into()), canceled: false }));
    // No installer (the test builds' default): an error, never a process.
    let (tx, _rx) = crossbeam_channel::unbounded();
    assert!(models::Download::start(&Installer::Disabled, &roots, "qwen3.8-27b", "Q8_0", Q8, None, tx).is_err());
}

/// Cancel stops the installer and everything it started (its process group) and removes its partial files.
#[test]
fn cancel_kills_the_installers_process_group_and_its_partials() {
    let (t, roots) = roots();
    fs::create_dir_all(&roots.hf).unwrap();
    let installer = Installer::Program(fake_installer(t.path(), "hang", None));
    let (tx, rx) = crossbeam_channel::unbounded();
    let d = models::Download::start(&installer, &roots, "qwen3.8-27b", "Q8_0", Q8, None, tx).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !t.path().join("child").exists() || fs::read_to_string(t.path().join("child")).unwrap().trim().is_empty() {
        assert!(Instant::now() < deadline, "the fake did not start");
        thread::sleep(Duration::from_millis(20));
    }
    let partial = roots.hf.join("models--huihui-ai--Huihui-Qwen3.8-27B-abliterated-GGUF/blobs/abc.incomplete");
    assert!(partial.exists());
    let pid: i32 = fs::read_to_string(t.path().join("pid")).unwrap().trim().parse().unwrap();
    let child: i32 = fs::read_to_string(t.path().join("child")).unwrap().trim().parse().unwrap();
    assert!(alive(pid) && alive(child));
    d.cancel();
    let events = events_until_done(&rx);
    assert!(matches!(events.last(), Some(DownloadEvent::Done { canceled: true, result: Err(_), .. })));
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(child) && Instant::now() < deadline { thread::sleep(Duration::from_millis(20)); }
    assert!(!alive(child), "the installer's child went with it");
    assert!(!partial.exists(), "its partial download went");
}

/// Heimdall quitting mid-download stops the installer but keeps its partial files, so the next download resumes.
#[test]
fn quitting_stops_the_installer_and_keeps_its_partials() {
    let (t, roots) = roots();
    fs::create_dir_all(&roots.hf).unwrap();
    let installer = Installer::Program(fake_installer(t.path(), "hang", None));
    let (tx, rx) = crossbeam_channel::unbounded();
    let d = models::Download::start(&installer, &roots, "qwen3.8-27b", "Q8_0", Q8, None, tx).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while fs::read_to_string(t.path().join("child")).map_or(true, |c| c.trim().is_empty()) {
        assert!(Instant::now() < deadline, "the fake did not start");
        thread::sleep(Duration::from_millis(20));
    }
    let child: i32 = fs::read_to_string(t.path().join("child")).unwrap().trim().parse().unwrap();
    drop(d);
    assert!(matches!(rx.try_iter().last(), Some(DownloadEvent::Done { canceled: true, .. })));
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(child) && Instant::now() < deadline { thread::sleep(Duration::from_millis(20)); }
    assert!(!alive(child));
    assert!(roots.hf.join("models--huihui-ai--Huihui-Qwen3.8-27B-abliterated-GGUF/blobs/abc.incomplete").exists());
}

/// Review 5f550efc #1: another link naming the same assembly keeps it and every file it links; only this link and its
/// pins go, and its manifest passes to the other link.
#[test]
fn deleting_a_link_that_shares_its_assembly_keeps_the_files() {
    let (_t, roots) = roots();
    let assembly = fixture::install_quant(&roots, Q4, DRAFT);
    let models = roots.models.canonicalize().unwrap();
    let other = models.join(".selections").join("a".repeat(64));
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    symlink(&assembly, &other).unwrap();
    let hf = roots.hf.canonicalize().unwrap();
    let before = files(&hf);
    let owner = models::pin_owner(&models.join(Q4));
    let deleted = models::delete(&roots, Q4).unwrap();
    assert!(!roots.models.join(Q4).exists() && assembly.exists());
    assert_eq!(deleted.bytes, 0);
    let gone: Vec<_> = before.difference(&files(&hf)).cloned().collect();
    assert!(!gone.is_empty() && gone.iter().all(|f| f.contains(&format!("refs/splash/{owner}/"))), "only its pins: {gone:?}");
    let rel = format!(".selections/{}", "a".repeat(64));
    assert_eq!(models::read_manifest(&roots, &rel).files.len(), 4, "the other link now owns what Heimdall downloaded");
    assert!(!models::manifest_path(&roots, Q4).exists());
}

/// Review 5f550efc #2: cache files Heimdall's pull did not download (no manifest, or not in it) are never deleted:
/// another tool sharing the cache may use them. The manifest is private and goes with the precision.
#[test]
fn delete_removes_only_cache_files_heimdalls_pull_downloaded() {
    let (_t, roots) = roots();
    // Another tool fetched the draft first; Heimdall's pull of Q4 then downloaded only the GGUF and projector.
    let hf = { fs::create_dir_all(&roots.hf).unwrap(); roots.hf.canonicalize().unwrap() };
    fixture::cache_entry(&hf, DRAFT, "config.json", format!("draft config {DRAFT}").as_bytes());
    fixture::cache_entry(&hf, DRAFT, "model.safetensors", format!("draft weights {DRAFT}").as_bytes());
    fixture::install_quant(&roots, Q4, DRAFT);
    let manifest = models::manifest_path(&roots, Q4);
    assert_eq!(fs::metadata(&manifest).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(models::read_manifest(&roots, Q4).files.iter().map(|f| f.file.as_str()).collect::<Vec<_>>(), ["Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf", "mmproj.gguf"]);
    let draft = blob_of(&roots, DRAFT, "model.safetensors");
    let projector = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "mmproj.gguf");
    let deleted = models::delete(&roots, Q4).unwrap();
    assert!(draft.exists(), "not Heimdall's download");
    assert!(!projector.exists(), "Heimdall's download, used by nothing else");
    assert!(deleted.bytes >= fs::metadata(&draft).unwrap().len());
    assert!(!manifest.exists());
    // An installation Heimdall did not pull at all: only its link, assembly, metadata and pins go.
    let assembly = fixture::install_quant_as(&roots, Q8, DRAFT, false);
    let before = files(&hf);
    let deleted = models::delete(&roots, Q8).unwrap();
    assert!(!assembly.exists() && !roots.models.join(Q8).exists());
    let gone: Vec<_> = before.difference(&files(&hf)).cloned().collect();
    assert!(gone.iter().all(|f| f.contains("/refs/splash/")), "no cache file: {gone:?}");
    assert!(deleted.bytes < 10_000, "only the assembly and metadata: {}", deleted.bytes);
}

/// Review 5f550efc #7: only 64-character lowercase hex names under .metadata are metadata entries.
#[test]
fn delete_keeps_metadata_paths_that_are_not_keys() {
    let (_t, roots) = roots();
    let assembly = fixture::install_quant(&roots, Q4, DRAFT);
    let meta = roots.models.canonicalize().unwrap().join(".metadata");
    for odd in ["ABCDEF".repeat(10) + "ABCD", "not-a-key".to_string()] {
        fs::create_dir_all(meta.join(&odd)).unwrap();
        fs::write(meta.join(&odd).join("config.json"), b"{}").unwrap();
    }
    let mut record: Value = serde_json::from_slice(&fs::read(assembly.join("model.json")).unwrap()).unwrap();
    record["files"]["tokenizer/upper.json"] = serde_json::json!({"path": meta.join("ABCDEF".repeat(10) + "ABCD").join("config.json").display().to_string()});
    record["files"]["tokenizer/odd.json"] = serde_json::json!({"path": meta.join("not-a-key").join("config.json").display().to_string()});
    fs::write(assembly.join("model.json"), serde_json::to_vec(&record).unwrap()).unwrap();
    models::delete(&roots, Q4).unwrap();
    assert!(meta.join("ABCDEF".repeat(10) + "ABCD").join("config.json").exists() && meta.join("not-a-key/config.json").exists());
    assert_eq!(fs::read_dir(&meta).unwrap().count(), 2, "its real key went");
}

/// Review 5f550efc #8: an installation running in another Splash's root (Homebrew's) refuses the delete.
#[test]
fn delete_waits_for_another_roots_installation() {
    let (_t, roots) = roots();
    fixture::install_quant(&roots, Q4, DRAFT);
    fs::create_dir_all(&roots.others[0]).unwrap();
    let lock = File::create(roots.others[0].join(".install.lock")).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    let e = models::delete(&roots, Q4).unwrap_err();
    assert!(format!("{e:#}").contains("another Splash is installing"), "{e:#}");
    assert!(models::installed(&roots.models, Q4));
    drop(lock);
    models::delete(&roots, Q4).unwrap();
    // A root without a lock file is not given one.
    let (_t2, roots) = self::roots();
    fixture::install_quant(&roots, Q4, DRAFT);
    models::delete(&roots, Q4).unwrap();
    assert!(!roots.others[0].join(".install.lock").exists());
}

/// A grandchild holding the installer's output pipe open never keeps the download from ending.
#[test]
fn a_lingering_grandchild_does_not_hold_the_download() {
    let (t, roots) = roots();
    let assembly = fixture::install_quant(&roots, Q4, DRAFT);
    fs::remove_file(roots.models.join(Q4)).unwrap();
    let installer = Installer::Program(fake_installer(t.path(), "linger", Some(&assembly)));
    let (tx, rx) = crossbeam_channel::unbounded();
    let started = Instant::now();
    let d = models::Download::start(&installer, &roots, "qwen3.8-27b", "UD-Q4_K_XL", Q4, None, tx).unwrap();
    let events = events_until_done(&rx);
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert!(matches!(events.last(), Some(DownloadEvent::Done { result: Ok(()), .. })));
    drop(d);
    let child: i32 = fs::read_to_string(t.path().join("child")).unwrap().trim().parse().unwrap();
    unsafe { libc::kill(child, libc::SIGKILL) };
}


/// Review 13e04dcb #1: a blob whose hub download lock another process holds (a pull in flight) stays, with its entry.
#[test]
fn delete_keeps_a_blob_whose_download_lock_is_held() {
    let (_t, roots) = roots();
    fixture::install_quant(&roots, Q4, DRAFT);
    let repo = "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF";
    let gguf = blob_of(&roots, repo, "Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf");
    let projector = blob_of(&roots, repo, "mmproj.gguf");
    let lock_path = roots.hf.join(".locks/models--huihui-ai--Huihui-Qwen3.8-27B-abliterated-GGUF").join(format!("{}.lock", gguf.file_name().unwrap().to_string_lossy()));
    fs::create_dir_all(lock_path.parent().unwrap()).unwrap();
    let lock = File::create(&lock_path).unwrap();
    assert_eq!(unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) }, 0);
    models::delete(&roots, Q4).unwrap();
    assert!(gguf.exists() && blob_of_exists(&roots, repo, "Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf"), "held: kept");
    assert!(!projector.exists(), "the rest goes");
}

/// Review 13e04dcb #2: an assembly in another root's `.resolved` with no link (a server may hold it) keeps its files.
#[test]
fn delete_keeps_files_an_unlinked_assembly_in_another_root_uses() {
    let (_t, roots) = roots();
    let ours = fixture::install_quant(&roots, Q4, DRAFT);
    let theirs = roots.others[0].join(".resolved").join("b".repeat(64));
    fs::create_dir_all(&theirs).unwrap();
    fs::copy(ours.join("model.json"), theirs.join("model.json")).unwrap();
    let gguf = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf");
    models::delete(&roots, Q4).unwrap();
    assert!(gguf.exists() && blob_of(&roots, DRAFT, "model.safetensors").exists());
    assert!(!ours.exists());
}

/// Review 13e04dcb #3: a cache file that is no longer the one the pull recorded (rewritten, replaced) is kept.
#[test]
fn delete_keeps_a_cache_file_changed_since_it_was_recorded() {
    let (_t, roots) = roots();
    fixture::install_quant(&roots, Q4, DRAFT);
    let entry = models::read_manifest(&roots, Q4).files.into_iter().find(|f| f.file.ends_with("UD-Q4_K_XL.gguf")).unwrap();
    assert!(entry.identity.is_some_and(|i| i.size == "weights of ".len() as u64 + Q4.len() as u64 && i.ino > 0));
    let gguf = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "Huihui-Qwen3.8-27B-abliterated-GGUF-UD-Q4_K_XL.gguf");
    let projector = blob_of(&roots, "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF", "mmproj.gguf");
    fs::write(&gguf, b"another tool's bytes").unwrap();
    models::delete(&roots, Q4).unwrap();
    assert_eq!(fs::read(&gguf).unwrap(), b"another tool's bytes");
    assert!(!projector.exists(), "unchanged files go");
    assert!(!models::manifest_path(&roots, Q4).exists(), "its entry went with the manifest");
}

/// Review 13e04dcb #4: writers of the same manifest at once never collide on a staging file.
#[test]
fn concurrent_manifest_writes_never_collide() {
    let (_t, roots) = roots();
    let roots = std::sync::Arc::new(roots);
    let threads: Vec<_> = (0..8).map(|n| {
        let roots = roots.clone();
        thread::spawn(move || {
            for k in 0..25 {
                let file = models::ManifestFile { repo: "o/r".into(), file: format!("{n}-{k}"), pointer: format!("/p/{n}/{k}").into(), blob: None, identity: None };
                models::write_manifest(&roots, &models::Manifest { source: Q4.into(), files: [file].into() }).unwrap();
            }
        })
    }).collect();
    for t in threads { t.join().unwrap(); }
    assert_eq!(models::read_manifest(&roots, Q4).files.len(), 1);
    assert_eq!(fs::read_dir(&roots.manifests).unwrap().count(), 1, "no staging file left");
}
