//! The sentinel tests UPD-T01..T22. Each host test runs against local fixture releases in a temp dir.

use super::fixture::*;
use crate::fetch::{Fetch, HttpFetch};
use crate::install::{self, BusyGate, Fault, Outcome, Slots};
use crate::readiness::{self, Phase};
use crate::release::{archive_entries_are_safe, expected_sha256};
use crate::skills;
use crate::source::Source;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tree(app: &Path) -> String {
    skills::tree_hash(app).unwrap()
}

/// A full update on the host: old installed, new published, prepared, installed, ready. Returns the outcome.
fn full_update(f: &Fixture, from: &str, to: &str, mode: &str) -> crate::Result<Outcome> {
    f.stop_all();
    let build = f.root.join(format!("build-{to}"));
    std::fs::create_dir_all(&build).unwrap();
    let new = f.make_app(&build, to, "1", mode, PRODUCT.bundle_id);
    let release = f.publish(&new, to);
    let staged = f.prepare(&release)?;
    f.installer(f.plan(staged, from)).run()
}

fn strings(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|p| p.display().to_string()).collect()
}

// UPD-T01: the isolated profile and its manifest.
#[test]
fn upd_t01_isolated_profile_and_baseline_inventory() {
    let f = Fixture::new();
    let sentinels = plant_sentinels(&f);
    let manifest = manifest(&sentinels);
    let path = f.root.join("manifest.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest.iter().map(|(p, v)| (p.display().to_string(), v)).collect::<Vec<_>>()).unwrap()).unwrap();
    assert!(path.is_file());
    assert_eq!(manifest.len(), sentinels.len());
    let real_support = PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support");
    for p in sentinels.iter().chain([&f.apps, &f.state, &f.data, &f.work]) {
        assert!(p.starts_with(&f.root), "{} is outside the temp root", p.display());
        assert!(!p.starts_with(&real_support), "{} is in the real Application Support", p.display());
        assert!(!p.starts_with("/Applications"));
    }
    assert!(f.root.starts_with(std::env::temp_dir().canonicalize().unwrap()));
}

// UPD-T02: model, Splash cache, engine.json and HF cache sentinels survive a full update.
#[test]
fn upd_t02_model_and_engine_path_sentinels_survive() {
    let f = Fixture::new();
    let sentinels = plant_sentinels(&f);
    let before = manifest(&sentinels);
    f.install_old("0.1.0-alpha.1");
    let outcome = full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.2", "ready").unwrap();
    assert!(matches!(outcome, Outcome::Ready(_)), "{outcome:?}");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.2");
    assert_eq!(manifest(&sentinels), before, "sentinels changed: {:?}", strings(&sentinels));
}

#[test]
#[ignore = "VM: the real new build must read <data>/splash/engine.json after the update (tests/vm/upd_vm.sh t02)"]
fn upd_t02_vm_new_build_reads_engine_json() {
    panic!("run tests/vm/upd_vm.sh t02 in the VM guest");
}

// UPD-T03: an archive row survives with its WAL left un-checkpointed; the pre-update snapshot holds it.
#[test]
fn upd_t03_memory_and_archive_db_row_survives() {
    use rusqlite::Connection;
    let f = Fixture::new();
    let archive = f.data.join("archive");
    std::fs::create_dir_all(archive.join("packs")).unwrap();
    let index = archive.join("heimdall.sqlite");
    let writer = Connection::open(&index).unwrap();
    writer.pragma_update(None, "journal_mode", "WAL").unwrap();
    writer.pragma_update(None, "wal_autocheckpoint", 0).unwrap();
    writer.execute_batch("CREATE TABLE sessions(name TEXT); INSERT INTO sessions VALUES ('upd-sentinel-session');").unwrap();
    let hot = Connection::open(archive.join("packs/hot.sqlite")).unwrap();
    hot.pragma_update(None, "journal_mode", "WAL").unwrap();
    hot.execute_batch("CREATE TABLE blocks(object TEXT); INSERT INTO blocks VALUES ('upd-sentinel-object');").unwrap();
    let wal = archive.join("heimdall.sqlite-wal");
    assert!(std::fs::metadata(&wal).unwrap().len() > 0, "the WAL holds the row");
    // The pre-update snapshot (SQLite online backup, as export::snapshot_archive does per file).
    let snapshot = f.root.join("snapshot.sqlite");
    {
        let mut dst = Connection::open(&snapshot).unwrap();
        rusqlite::backup::Backup::new(&writer, &mut dst).unwrap().run_to_completion(64, std::time::Duration::ZERO, None).unwrap();
    }
    f.install_old("0.1.0-alpha.1");
    assert!(matches!(full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.2", "ready").unwrap(), Outcome::Ready(_)));
    assert!(wal.exists(), "the WAL was not deleted");
    let reader = Connection::open(&index).unwrap();
    let n: i64 = reader.query_row("SELECT count(*) FROM sessions WHERE name='upd-sentinel-session'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    let ok: String = reader.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(ok, "ok");
    let blocks: i64 = Connection::open(archive.join("packs/hot.sqlite")).unwrap()
        .query_row("SELECT count(*) FROM blocks WHERE object='upd-sentinel-object'", [], |r| r.get(0)).unwrap();
    assert_eq!(blocks, 1);
    let snap: i64 = Connection::open(&snapshot).unwrap()
        .query_row("SELECT count(*) FROM sessions WHERE name='upd-sentinel-session'", [], |r| r.get(0)).unwrap();
    assert_eq!(snap, 1);
    drop(writer);
}

// UPD-T04: settings.json, with a known value and an unknown future key, is byte-identical after the update.
// The app's own save of settings (crates/app/src/settings.rs) drops keys it does not know (docs/install.md); that
// is a known risk outside the updater, recorded in docs/updates.md.
#[test]
fn upd_t04_settings_value_and_unknown_key_survive() {
    let f = Fixture::new();
    plant_sentinels(&f);
    let settings = f.data.join("settings.json");
    let before = crate::sha256_file(&settings).unwrap();
    f.install_old("0.1.0-alpha.1");
    full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.2", "ready").unwrap();
    assert_eq!(crate::sha256_file(&settings).unwrap(), before);
    let json: serde_json::Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(json["ack_delay_ms"], 1500);
    assert_eq!(json["upd_sentinel_key"], "keep");
}

// UPD-T05: Keychain items follow the signing identity; only a VM guest has a disposable login session.
#[test]
#[ignore = "VM: throwaway keychain item 'Heimdall API provider: UPD-SENTINEL' survives the update (tests/vm/upd_vm.sh t05)"]
fn upd_t05_keychain_reference_survives() {
    panic!("run tests/vm/upd_vm.sh t05 in the VM guest");
}

// UPD-T06: a skill the user made is never touched.
#[test]
fn upd_t06_custom_skill_survives() {
    let f = Fixture::new();
    let (bundle, profile, store) = skill_dirs(&f);
    write_skill(&bundle.join("public/upd-bundled-a"), "a-v1");
    write_skill(&profile.join("upd-custom"), "custom-v1");
    let before = crate::sha256_file(&profile.join("upd-custom/SKILL.md")).unwrap();
    skills::upgrade(&[bundle.join("public")], &profile, &store, "0.1.0-alpha.2").unwrap();
    assert_eq!(crate::sha256_file(&profile.join("upd-custom/SKILL.md")).unwrap(), before);
    let baseline: serde_json::Value = serde_json::from_slice(&std::fs::read(store.join(skills::BASELINE)).unwrap()).unwrap();
    assert!(baseline.get("upd-custom").is_none());
    assert!(!store.join(skills::INCOMING).join("upd-custom").exists());
}

fn skill_dirs(f: &Fixture) -> (PathBuf, PathBuf, PathBuf) {
    (f.root.join("bundle-skills"), f.data.join("agent/skills"), f.data.join("updates"))
}

fn write_skill(dir: &Path, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

// UPD-T07: an unmodified bundled skill takes the new version.
#[test]
fn upd_t07_unmodified_bundled_skill_is_updated() {
    let f = Fixture::new();
    let (bundle, profile, store) = skill_dirs(&f);
    let old = bundle.join("v1");
    let new = bundle.join("v2");
    write_skill(&old.join("upd-bundled-a"), "a-v1");
    write_skill(&profile.join("upd-bundled-a"), "a-v1"); // seeded by the old build
    skills::upgrade(&[old], &profile, &store, "0.1.0-alpha.1").unwrap();
    write_skill(&new.join("upd-bundled-a"), "a-v2");
    let report = skills::upgrade(&[new.clone()], &profile, &store, "0.1.0-alpha.2").unwrap();
    assert_eq!(report.updated, ["upd-bundled-a"]);
    assert!(report.conflicts.is_empty());
    assert_eq!(std::fs::read_to_string(profile.join("upd-bundled-a/SKILL.md")).unwrap(), "a-v2");
    let baseline: serde_json::Value = serde_json::from_slice(&std::fs::read(store.join(skills::BASELINE)).unwrap()).unwrap();
    assert_eq!(baseline["upd-bundled-a"], skills::tree_hash(&new.join("upd-bundled-a")).unwrap());
    assert!(!profile.join(".upd-bundled-a.old").exists() && !profile.join(".upd-bundled-a.part").exists());
}

// UPD-T08: an edited bundled skill is kept; the incoming copy is saved outside the loaded tree; one notice.
#[test]
fn upd_t08_edited_bundled_skill_kept_incoming_saved_notice_recorded() {
    let f = Fixture::new();
    let (bundle, profile, store) = skill_dirs(&f);
    write_skill(&bundle.join("v1/upd-bundled-b"), "b-v1");
    write_skill(&profile.join("upd-bundled-b"), "b-v1");
    skills::upgrade(&[bundle.join("v1")], &profile, &store, "0.1.0-alpha.1").unwrap();
    write_skill(&profile.join("upd-bundled-b"), "edited");
    let edited = crate::sha256_file(&profile.join("upd-bundled-b/SKILL.md")).unwrap();
    write_skill(&bundle.join("v2/upd-bundled-b"), "b-v2");
    for _start in 0..2 {
        skills::upgrade(&[bundle.join("v2")], &profile, &store, "0.1.0-alpha.2").unwrap();
    }
    assert_eq!(crate::sha256_file(&profile.join("upd-bundled-b/SKILL.md")).unwrap(), edited);
    let incoming = store.join(skills::INCOMING).join("upd-bundled-b/SKILL.md");
    assert_eq!(std::fs::read_to_string(incoming).unwrap(), "b-v2");
    assert!(!store.starts_with(&profile));
    let notices: Vec<_> = skills::pending(&store).into_iter().filter(|n| n.release == "0.1.0-alpha.2").collect();
    assert_eq!(notices.len(), 1, "a second start does not duplicate the notice");
    assert_eq!(notices[0].conflicts, ["upd-bundled-b"]);
}

// UPD-T09: the notice is written before any change, survives restarts until acknowledged, and is delivered once;
// a release with nothing to report still gets its explicit empty notice.
#[test]
fn upd_t09_notice_delivery_is_durable_and_idempotent() {
    let f = Fixture::new();
    let (bundle, profile, store) = skill_dirs(&f);
    write_skill(&bundle.join("public/upd-bundled-a"), "a-v1");
    write_skill(&profile.join("upd-bundled-a"), "a-v1");
    let first = skills::upgrade(&[bundle.join("public")], &profile, &store, "0.1.0-alpha.2").unwrap();
    assert!(first.notice_written);
    // Killed before the ack, restarted twice.
    for _ in 0..2 {
        let again = skills::upgrade(&[bundle.join("public")], &profile, &store, "0.1.0-alpha.2").unwrap();
        assert!(!again.notice_written);
    }
    let pending = skills::pending(&store);
    assert_eq!(pending.len(), 1);
    assert!(pending[0].updated.is_empty() && pending[0].conflicts.is_empty(), "explicit no-changes notice");
    skills::acknowledge(&store, "0.1.0-alpha.2").unwrap();
    skills::acknowledge(&store, "0.1.0-alpha.2").unwrap();
    assert!(skills::pending(&store).is_empty());
    skills::upgrade(&[bundle.join("public")], &profile, &store, "0.1.0-alpha.2").unwrap();
    assert!(skills::pending(&store).is_empty(), "an acknowledged notice is never re-sent");
}

/// Serves `files` over plain HTTP on the loopback address; `truncate` names a file whose body stops halfway after
/// a full Content-Length header.
fn serve(files: Vec<(String, Vec<u8>)>, truncate: Option<String>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let Some((name, body)) = files.iter().find(|(name, _)| path.ends_with(&format!("/{name}"))) else {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                continue;
            };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let cut = if truncate.as_deref() == Some(name.as_str()) { body.len() / 2 } else { body.len() };
            let _ = stream.write_all(&body[..cut]);
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
}

// UPD-T10: a download cut off halfway leaves the installed app unchanged and nothing behind.
#[test]
fn upd_t10_interrupted_download_leaves_the_installed_app_unchanged() {
    let f = Fixture::new();
    let installed = f.install_old("0.1.0-alpha.1");
    let before = tree(&installed);
    let build = f.root.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let release = f.publish(&f.make_app(&build, "0.1.0-alpha.2", "1", "ready", PRODUCT.bundle_id), "0.1.0-alpha.2");
    let dir = f.releases.join(&release.tag);
    let zip = PRODUCT.zip_name(&release.version);
    let files = ["SHA256SUMS", "SHA256SUMS.sig", zip.as_str()]
        .iter()
        .map(|n| (n.to_string(), std::fs::read(dir.join(n)).unwrap()))
        .collect();
    let base = serve(files, Some(zip.clone()));
    let source = Source { list_url: format!("{base}/releases.json"), base: Some(base) };
    let error = f.prepare_with(&release, &HttpFetch::new().unwrap(), source).unwrap_err();
    assert!(error.0.contains("download") || error.0.contains("ended early"), "{error}");
    assert_eq!(tree(&installed), before);
    assert!(f.work_is_empty(), "staging removed");
    assert!(f.txn_entries().is_empty(), "no .heimdall-update left");
    // A fetcher that fails mid-stream is refused the same way.
    struct Broken;
    impl Fetch for Broken {
        fn get(&self, url: &str, max: u64) -> crate::Result<Vec<u8>> {
            HttpFetch::new().unwrap().get(url, max)
        }
        fn download(&self, _: &str, dest: &Path, _: u64) -> crate::Result<()> {
            std::fs::write(dest, b"partial").unwrap();
            Err(crate::UpdateError::new("download of the zip failed: connection reset"))
        }
    }
    assert!(f.prepare_with(&release, &Broken, f.source()).is_err());
    assert!(f.work_is_empty());
    assert_eq!(tree(&installed), before);
}

// UPD-T11: a flipped byte (SHA-256 mismatch), another signing identity, and a checksum file signed by an untrusted
// key are refused before anything is stopped or moved.
#[test]
fn upd_t11_corrupted_payload_is_refused_before_any_swap() {
    let f = Fixture::new();
    let installed = f.install_old("0.1.0-alpha.1");
    let before = tree(&installed);
    let build = f.root.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let release = f.publish(&f.make_app(&build, "0.1.0-alpha.2", "1", "ready", PRODUCT.bundle_id), "0.1.0-alpha.2");
    let zip = f.releases.join(&release.tag).join(PRODUCT.zip_name(&release.version));
    let mut bytes = std::fs::read(&zip).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&zip, &bytes).unwrap();
    let error = f.prepare(&release).unwrap_err();
    assert!(error.0.contains("SHA-256 mismatch"), "{error}");
    assert!(f.work_is_empty() && f.txn_entries().is_empty());
    // Variant 2: correct hash, but the app is signed as another identity.
    let other = f.make_app(&build, "0.1.0-alpha.2", "1", "ready", "com.example.other");
    let release = f.publish(&other, "0.1.0-alpha.2");
    let error = f.prepare(&release).unwrap_err();
    assert!(error.0.contains("different identity"), "{error}");
    // Variant 3: SHA256SUMS signed by a key the app does not trust.
    let good = f.make_app(&build, "0.1.0-alpha.2", "1", "ready", PRODUCT.bundle_id);
    let release = f.publish(&good, "0.1.0-alpha.2");
    let dir = f.releases.join(&release.tag);
    let sums = std::fs::read(dir.join("SHA256SUMS")).unwrap();
    std::fs::write(dir.join("SHA256SUMS.sig"), crate::signing::sign(&crate::signing::generate_seed().unwrap(), &sums)).unwrap();
    assert!(f.prepare(&release).unwrap_err().0.contains("does not trust"));
    // Variant 4: SHA256SUMS edited to match a tampered zip, signature left as it was.
    let release = f.publish(&good, "0.1.0-alpha.2");
    let text = String::from_utf8(std::fs::read(dir.join("SHA256SUMS")).unwrap()).unwrap();
    let flipped = if text.starts_with('0') { "1" } else { "0" };
    std::fs::write(dir.join("SHA256SUMS"), format!("{flipped}{}", &text[1..])).unwrap();
    assert!(f.prepare(&release).unwrap_err().0.contains("signature is not valid"));
    assert!(f.work_is_empty() && f.txn_entries().is_empty());
    assert_eq!(tree(&installed), before);
    assert!(f.running().is_empty(), "nothing was started or stopped");
}

#[test]
#[ignore = "VM: two certificate identities (release and another self-signed certificate) need a signed build (tests/vm/upd_vm.sh t11)"]
fn upd_t11_vm_certificate_identity_mismatch() {
    panic!("run tests/vm/upd_vm.sh t11 in the VM guest");
}

// UPD-T12: SHA256SUMS without a line for the zip, or with two, refuses.
#[test]
fn upd_t12_missing_or_ambiguous_checksum_refuses() {
    let h = "a".repeat(64);
    assert_eq!(expected_sha256(&format!("{h}  x.zip\n"), "x.zip"), Some(h.clone()));
    assert_eq!(expected_sha256(&format!("{h} *x.zip\n"), "x.zip"), Some(h.clone()));
    assert_eq!(expected_sha256(&format!("{h}  y.zip\n"), "x.zip"), None);
    assert_eq!(expected_sha256(&format!("{h}  x.zip\n{h}  x.zip\n"), "x.zip"), None);
    assert_eq!(expected_sha256("abc  x.zip\n", "x.zip"), None);
    let f = Fixture::new();
    let installed = f.install_old("0.1.0-alpha.1");
    let before = tree(&installed);
    let build = f.root.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let app = f.make_app(&build, "0.1.0-alpha.2", "1", "ready", PRODUCT.bundle_id);
    let release = f.publish(&app, "0.1.0-alpha.2");
    let dir = f.releases.join(&release.tag);
    let zip = dir.join(PRODUCT.zip_name(&release.version));
    for sums in [format!("{h}  other.zip\n"), {
        let line = format!("{}  {}\n", crate::sha256_file(&zip).unwrap(), zip.file_name().unwrap().to_string_lossy());
        line.repeat(2)
    }] {
        std::fs::write(dir.join("SHA256SUMS"), &sums).unwrap();
        std::fs::write(dir.join("SHA256SUMS.sig"), crate::signing::sign(&f.seed, sums.as_bytes())).unwrap();
        assert!(f.prepare(&release).unwrap_err().0.contains("missing or ambiguous"));
        assert!(f.work_is_empty());
    }
    assert_eq!(tree(&installed), before);
}

/// A zip with exactly these entries (written by Python's zipfile, which keeps any name).
fn crafted_zip(path: &Path, entries: &[&str]) {
    let script = "import sys, zipfile\nz = zipfile.ZipFile(sys.argv[1], 'w')\nfor n in sys.argv[2:]: z.writestr(n, 'x')\nz.close()";
    run(Command::new("/usr/bin/python3").arg("-I").arg("-c").arg(script).arg(path).args(entries));
}

// UPD-T13: entries outside the app, `..`, backslashes, AppleDouble and __MACOSX refuse before unpacking.
#[test]
fn upd_t13_unsafe_archive_entries_refuse() {
    let app = PRODUCT.app_folder;
    let ok: Vec<String> = [format!("{app}/"), format!("{app}/Contents/Info.plist")].into();
    assert!(archive_entries_are_safe(&ok, app));
    for bad in [
        format!("{app}/../evil"),
        format!("{app}/Contents/..\\evil"),
        format!("{app}/Contents/._Info.plist"),
        "__MACOSX/x".to_owned(),
        format!("{app}/__MACOSX/x"),
        "evil.txt".to_owned(),
        "Heimdall Alpha.appx/x".to_owned(),
    ] {
        assert!(!archive_entries_are_safe(&[ok[0].clone(), bad.clone()], app), "{bad}");
    }
    assert!(!archive_entries_are_safe(&[], app));
    assert!(!archive_entries_are_safe(&vec![format!("{app}/x"); crate::release::MAX_ENTRIES + 1], app));
    let f = Fixture::new();
    let installed = f.install_old("0.1.0-alpha.1");
    let before = tree(&installed);
    let build = f.root.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let release = f.publish(&f.make_app(&build, "0.1.0-alpha.2", "1", "ready", PRODUCT.bundle_id), "0.1.0-alpha.2");
    let dir = f.releases.join(&release.tag);
    let zip = dir.join(PRODUCT.zip_name(&release.version));
    for extra in ["../escaped.txt", "__MACOSX/._x", "beside.txt", "Heimdall Alpha.app/Contents/._Info.plist"] {
        std::fs::remove_file(&zip).unwrap();
        crafted_zip(&zip, &["Heimdall Alpha.app/Contents/Info.plist", extra]);
        f.sign_sums(&dir, &[&zip]);
        let error = f.prepare(&release).unwrap_err();
        assert!(error.0.contains("unsafe or unexpected archive entries"), "{extra}: {error}");
        assert!(f.work_is_empty());
        assert!(!f.root.join("escaped.txt").exists() && !f.work.join("escaped.txt").exists());
    }
    assert_eq!(tree(&installed), before);
}

fn staged_from(f: &Fixture, version: &str, build: &str, identifier: &str) -> crate::prepare::Staged {
    let dir = crate::prepare::make_work_dir(&f.work).unwrap();
    let app = f.make_app(&dir.join("unpacked"), version, build, "ready", identifier);
    crate::prepare::Staged { version: version.into(), directory: dir, app, sha256: String::new() }
}

// UPD-T14: a copy failure (disk full) before the swap reports, removes the partial copy and leaves the installed app.
#[test]
fn upd_t14_disk_full_during_staging_or_copy() {
    let f = Fixture::new();
    let installed = f.install_old("0.1.0-alpha.1");
    let before = tree(&installed);
    let staged = staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id);
    let mut installer = f.installer(f.plan(staged, "0.1.0-alpha.1"));
    installer.copy = Box::new(|_, to| {
        std::fs::create_dir_all(to.join("Contents")).unwrap();
        std::fs::write(to.join("Contents/partial"), b"x").unwrap();
        Err(crate::UpdateError::new("No space left on device (os error 28)"))
    });
    let error = installer.run().unwrap_err();
    assert!(error.0.contains("No space left"), "{error}");
    assert_eq!(tree(&installed), before, "no move of the installed app");
    let slots = Slots::for_destination(&f.destination());
    assert!(!slots.incoming.exists() && !slots.previous.exists() && !slots.journal.exists());
    let result = install::take_result(&f.state).unwrap();
    assert!(!result.ok && result.message.contains("No space left"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!f.running().is_empty(), "the installed app was started again");
}

#[test]
#[ignore = "VM: a real ENOSPC on a small disk image during the copy (tests/vm/upd_vm.sh t14)"]
fn upd_t14_vm_real_enospc() {
    panic!("run tests/vm/upd_vm.sh t14 in the VM guest");
}

// UPD-T15: a crash between the two renames is recovered at the next start: the previous app comes back; a crash
// after both renames rolls forward with the previous app as the rollback. The data folder is untouched.
#[test]
fn upd_t15_crash_between_the_two_swap_renames_recovers() {
    let f = Fixture::new();
    let sentinels = plant_sentinels(&f);
    let data_before = manifest(&sentinels);
    f.install_old("0.1.0-alpha.1");
    let slots = Slots::for_destination(&f.destination());

    let mut installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id), "0.1.0-alpha.1"));
    installer.fault = Fault::CrashBetweenRenames;
    assert!(installer.run().is_err());
    assert!(!f.destination().exists() && slots.previous.exists() && slots.journal.exists(), "crashed mid-swap");
    let note = install::recover(&f.destination(), &f.state, &PRODUCT).unwrap().unwrap();
    assert!(note.contains("restored"), "{note}");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.1");
    assert!(!slots.journal.exists() && !slots.incoming.exists() && !slots.previous.exists());
    assert_eq!(install::recover(&f.destination(), &f.state, &PRODUCT).unwrap(), None, "idempotent");

    let mut installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id), "0.1.0-alpha.1"));
    installer.fault = Fault::CrashAfterSwap;
    assert!(installer.run().is_err());
    let note = install::recover(&f.destination(), &f.state, &PRODUCT).unwrap().unwrap();
    assert!(note.contains("kept"), "{note}");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.2");
    assert_eq!(f.version_at(&slots.rollback), "0.1.0-alpha.1");
    assert_eq!(f.txn_entries(), ["rollback"]);

    // An error (not a crash) between the renames undoes the swap at once.
    let mut installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.3", "1", PRODUCT.bundle_id), "0.1.0-alpha.2"));
    installer.fault = Fault::ErrorBetweenRenames;
    assert!(installer.run().unwrap_err().0.contains("injected"));
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.2");
    assert!(!slots.journal.exists() && !slots.previous.exists());
    assert_eq!(manifest(&sentinels), data_before);

    // Recovery waits while an install tool holds the lock.
    f.stop_all();
    let mut installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.3", "1", PRODUCT.bundle_id), "0.1.0-alpha.2"));
    installer.fault = Fault::CrashBetweenRenames;
    let _ = installer.run();
    let held = install::Lock::try_take(&Slots::for_destination(&f.destination()).txn).unwrap().unwrap();
    assert_eq!(install::recover(&f.destination(), &f.state, &PRODUCT).unwrap(), None);
    drop(held);
    assert!(install::recover(&f.destination(), &f.state, &PRODUCT).unwrap().is_some());
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.2");
}

#[test]
#[ignore = "VM: SIGKILL of heimdall-install-tool between the renames, then a relaunch (tests/vm/upd_vm.sh t15)"]
fn upd_t15_vm_real_sigkill() {
    panic!("run tests/vm/upd_vm.sh t15 in the VM guest");
}

// UPD-T16: a new version that exits at launch is rolled back: the previous app is back and running, the failed
// bundle is gone, the result says why, the data is untouched.
#[test]
fn upd_t16_failed_first_launch_rolls_back() {
    // State: no answer within the start timeout, on a fake clock.
    let f = Fixture::new();
    f.install_old("0.1.0-alpha.1");
    let mut installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id), "0.1.0-alpha.1"));
    installer.start_timeout = 120.0;
    installer.clock = Box::new(FakeClock { now: 0.0.into(), on_tick: Box::new(|_| {}) });
    assert_eq!(installer.await_ready(), Outcome::Failed("no answer after 120 s".into()));

    // Host: a real launch of a fixture that exits 1.
    let sentinels = plant_sentinels(&f);
    let before = manifest(&sentinels);
    let error = full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.2", "exit1").unwrap_err();
    assert!(error.0.contains("did not become ready") && error.0.contains("restored"), "{error}");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.1");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!f.running().is_empty(), "the previous version runs again");
    let result = install::take_result(&f.state).unwrap();
    assert!(!result.ok && result.message.contains("no answer"), "{}", result.message);
    let slots = Slots::for_destination(&f.destination());
    assert!(!slots.previous.exists() && !slots.journal.exists());
    assert!(f.txn_entries().iter().all(|e| !e.starts_with("failed-")), "failed bundle not kept");
    assert!(!slots.rollback.exists(), "the failed version is never the rollback");
    assert_eq!(manifest(&sentinels), before);
}

#[test]
#[ignore = "VM: a certificate-signed build that fails at launch, installed in /Applications (tests/vm/upd_vm.sh t16)"]
fn upd_t16_vm_signed_failed_launch() {
    panic!("run tests/vm/upd_vm.sh t16 in the VM guest");
}

const T17_VERSION: &str = "0.1.0-alpha.2";

fn status(phase: Phase) -> Vec<u8> {
    serde_json::to_vec(&readiness::Status { app_pid: 4242, version: T17_VERSION.into(), phase, detail: "model x failed".into() }).unwrap()
}

// UPD-T17: a launch-set model failing for less than the settle time keeps waiting; past it, the update fails; a
// memory refusal (degraded) keeps the new version.
#[test]
fn upd_t17_launch_set_model_failure_settles_then_rolls_back() {
    let f = Fixture::new();
    f.install_old("0.1.0-alpha.1");
    let version = T17_VERSION;
    let make = |script: Vec<(f64, Phase)>| {
        let mut installer = f.installer(f.plan(staged_from(&f, version, "1", PRODUCT.bundle_id), "0.1.0-alpha.1"));
        installer.is_installed_app = Box::new(|pid, _| pid == 4242);
        installer.settle = 60.0;
        installer.poll = 1.0;
        let file = readiness::path(&f.state);
        let first = script[0].1;
        std::fs::write(&file, status(first)).unwrap();
        installer.clock = Box::new(FakeClock {
            now: 0.0.into(),
            on_tick: Box::new(move |now| {
                if let Some((_, phase)) = script.iter().rev().find(|(at, _)| now >= *at) {
                    std::fs::write(&file, status(*phase)).unwrap();
                }
            }),
        });
        installer
    };
    // Failing for 30 s, then ready: never failed.
    let outcome = make(vec![(0.0, Phase::Failing), (30.0, Phase::Ready)]).await_ready();
    assert!(matches!(outcome, Outcome::Ready(_)), "{outcome:?}");
    // Failing on and off resets the settle timer.
    let outcome = make(vec![(0.0, Phase::Failing), (50.0, Phase::Starting), (55.0, Phase::Failing), (100.0, Phase::Ready)]).await_ready();
    assert!(matches!(outcome, Outcome::Ready(_)), "{outcome:?}");
    // Failing past the settle time: failed, which rolls back.
    let outcome = make(vec![(0.0, Phase::Failing)]).await_ready();
    assert_eq!(outcome, Outcome::Failed("model x failed".into()));
    // Degraded (refused for memory): the new version stays.
    assert!(matches!(make(vec![(0.0, Phase::Degraded)]).await_ready(), Outcome::Degraded(_)));
    // A status from another pid or version does not count.
    let mut other = make(vec![(0.0, Phase::Ready)]);
    other.is_installed_app = Box::new(|_, _| false);
    other.start_timeout = 10.0;
    assert!(matches!(other.await_ready(), Outcome::Failed(_)));

    // Host: a fixture that reports failing rolls back after the settle time.
    let error = full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.2", "failing").unwrap_err();
    assert!(error.0.contains("restored"), "{error}");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.1");
    // A degraded fixture is kept.
    assert!(matches!(full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.3", "degraded").unwrap(), Outcome::Degraded(_)));
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.3");
}

// UPD-T18: an older version or build refuses before anything is stopped or moved; the override never bypasses
// signing.
#[test]
fn upd_t18_downgrade_refused_before_any_stop() {
    let f = Fixture::new();
    f.make_app(&f.apps, "1.0.1", "2", "ready", PRODUCT.bundle_id);
    let before = tree(&f.destination());
    for (version, build) in [("1.0.0", "9"), ("1.0.1", "1"), ("1.0.1-alpha.9", "9")] {
        let mut installer = f.installer(f.plan(staged_from(&f, version, build, PRODUCT.bundle_id), "1.0.1"));
        installer.terminate = Box::new(|_| panic!("nothing may be stopped"));
        let error = installer.run().unwrap_err();
        assert!(error.0.contains("refusing to replace"), "{version}: {error}");
        assert_eq!(tree(&f.destination()), before);
        assert!(f.txn_entries().is_empty());
    }
    // With the override and a bad signature: still refused.
    f.stop_all();
    let mut plan = f.plan(staged_from(&f, "1.0.0", "9", "com.example.other"), "1.0.1");
    plan.allow_downgrade = true;
    let error = f.installer(plan).run().unwrap_err();
    assert!(error.0.contains("another identity"), "{error}");
    assert_eq!(tree(&f.destination()), before);
    // With the override and the same identity: allowed.
    f.stop_all();
    let mut plan = f.plan(staged_from(&f, "1.0.0", "9", PRODUCT.bundle_id), "1.0.1");
    plan.allow_downgrade = true;
    assert!(matches!(f.installer(plan).run().unwrap(), Outcome::Ready(_)));
    assert_eq!(f.version_at(&f.destination()), "1.0.0");
}

// UPD-T19: a busy app refuses at the hand-off; a running app, a linked destination or a held lock refuse at the
// swap. Nothing is stopped or moved.
#[test]
fn upd_t19_busy_app_refuses_before_swap() {
    struct Gate(Option<&'static str>);
    impl BusyGate for Gate {
        fn busy(&self) -> Option<String> {
            self.0.map(str::to_owned)
        }
    }
    let f = Fixture::new();
    f.install_old("0.1.0-alpha.1");
    let before = tree(&f.destination());
    let staged = staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id);
    let plan = f.plan(staged.clone(), "0.1.0-alpha.1");
    let error = install::hand_off_when_idle(&plan, &f.destination(), &Gate(Some("recording"))).unwrap_err();
    assert!(error.0.contains("recording"));
    assert!(!staged.directory.join(install::PLAN).exists(), "no plan written, no tool started");

    // The app was started again before the swap.
    let installer = f.installer(plan.clone());
    (installer.launch)(&f.destination()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    let error = f.installer(plan.clone()).run().unwrap_err();
    assert!(error.0.contains("running again"), "{error}");
    f.stop_all();
    assert_eq!(tree(&f.destination()), before);

    // Another install holds the lock.
    let held = install::Lock::try_take(&Slots::for_destination(&f.destination()).txn).unwrap().unwrap();
    let staged = staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id);
    assert!(f.installer(f.plan(staged, "0.1.0-alpha.1")).run().unwrap_err().0.contains("another update"));
    drop(held);
    let result = install::take_result(&f.state).unwrap();
    assert!(result.message.contains("another update"), "the refusal is reported");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!f.running().is_empty(), "the app that quit to hand off is started again");
    f.stop_all();

    // A linked destination.
    let real = f.root.join("elsewhere");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::rename(f.destination(), real.join(PRODUCT.app_folder)).unwrap();
    std::os::unix::fs::symlink(real.join(PRODUCT.app_folder), f.destination()).unwrap();
    let staged = staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id);
    assert!(f.installer(f.plan(staged, "0.1.0-alpha.1")).run().unwrap_err().0.contains("linked"));
    assert_eq!(tree(&real.join(PRODUCT.app_folder)), before);
    assert!(std::fs::symlink_metadata(f.destination()).unwrap().file_type().is_symlink());
}

// UPD-T20: the full update end to end on the host (file assertions): sentinels, the designated requirement, the
// previous copy kept until ready then exactly one rollback, skill outcomes and one release notice.
#[test]
fn upd_t20_full_update_preserves_all_sentinels_end_to_end() {
    let f = Fixture::new();
    let sentinels = plant_sentinels(&f);
    let before = manifest(&sentinels);
    let (bundle, profile, store) = skill_dirs(&f);
    write_skill(&bundle.join("v1/upd-bundled-a"), "a-v1");
    write_skill(&bundle.join("v1/upd-bundled-b"), "b-v1");
    write_skill(&profile.join("upd-bundled-a"), "a-v1");
    write_skill(&profile.join("upd-bundled-b"), "b-v1");
    skills::upgrade(&[bundle.join("v1")], &profile, &store, "0.1.0-alpha.1").unwrap();
    write_skill(&profile.join("upd-bundled-b"), "edited");
    write_skill(&bundle.join("v2/upd-bundled-a"), "a-v2");
    write_skill(&bundle.join("v2/upd-bundled-b"), "b-v2");
    skills::acknowledge(&store, "0.1.0-alpha.1").unwrap();

    f.install_old("0.1.0-alpha.1");
    // An older rollback from an earlier update, which this update replaces.
    let slots = Slots::for_destination(&f.destination());
    f.make_app(slots.rollback.parent().unwrap(), "0.0.9", "1", "ready", PRODUCT.bundle_id);
    let requirement = crate::bundle::verify_signed(&f.destination()).unwrap();

    let build = f.root.join("build");
    std::fs::create_dir_all(&build).unwrap();
    let release = f.publish(&f.make_app(&build, "0.1.0-alpha.2", "1", "ready", PRODUCT.bundle_id), "0.1.0-alpha.2");
    // Discovery through the release list, as the app does.
    let list = HttpFetch::new().unwrap().get(&f.source().list_url, crate::fetch::MAX_LIST).unwrap();
    let offer = crate::release::choose(&crate::Release::parse_list(&list).unwrap(), &crate::Version::parse("0.1.0-alpha.1").unwrap(), &PRODUCT).unwrap();
    assert_eq!(offer, release);
    let staged = f.prepare(&offer).unwrap();
    assert!(!crate::bundle::is_quarantined(&staged.app));
    let mut installer = f.installer(f.plan(staged.clone(), "0.1.0-alpha.1"));
    let previous_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let launch = std::mem::replace(&mut installer.launch, Box::new(|_| Ok(())));
    let (seen, previous) = (previous_seen.clone(), slots.previous.clone());
    installer.launch = Box::new(move |app| {
        seen.store(previous.exists(), std::sync::atomic::Ordering::SeqCst);
        launch(app)
    });
    assert!(matches!(installer.run().unwrap(), Outcome::Ready(_)));
    assert!(previous_seen.load(std::sync::atomic::Ordering::SeqCst), "previous kept until ready");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.2");
    assert_eq!(f.version_at(&slots.rollback), "0.1.0-alpha.1", "exactly one rollback: the version replaced");
    assert_eq!(f.txn_entries(), ["rollback"]);
    assert!(!staged.directory.exists(), "staging removed");
    assert_eq!(crate::bundle::verify_signed(&f.destination()).unwrap(), requirement);
    assert!(install::take_result(&f.state).is_none(), "a successful update leaves no result");
    // The new build's first agent start.
    let report = skills::upgrade(&[bundle.join("v2")], &profile, &store, "0.1.0-alpha.2").unwrap();
    assert_eq!((report.updated.as_slice(), report.conflicts.as_slice()), (&["upd-bundled-a".to_owned()][..], &["upd-bundled-b".to_owned()][..]));
    skills::upgrade(&[bundle.join("v2")], &profile, &store, "0.1.0-alpha.2").unwrap();
    assert_eq!(skills::pending(&store).len(), 1, "one release notice");
    assert_eq!(std::fs::read_to_string(profile.join("upd-bundled-b/SKILL.md")).unwrap(), "edited");
    assert_eq!(manifest(&sentinels), before);
}

#[test]
#[ignore = "VM: end to end in a guest with TCC grants, a keychain item and Launch at Login (tests/vm/upd_vm.sh t20)"]
fn upd_t20_vm_end_to_end_with_permissions() {
    panic!("run tests/vm/upd_vm.sh t20 in the VM guest");
}

// UPD-T21: going back to the rollback keeps every data file, including keys only the newer build wrote; the newer
// version becomes the rollback.
#[test]
fn upd_t21_rollback_restores_data_compatibly() {
    let f = Fixture::new();
    let sentinels = plant_sentinels(&f);
    f.install_old("0.1.0-alpha.1");
    assert!(matches!(full_update(&f, "0.1.0-alpha.1", "0.1.0-alpha.2", "ready").unwrap(), Outcome::Ready(_)));
    // The newer build writes a newer settings key.
    let settings = f.data.join("settings.json");
    std::fs::write(&settings, br#"{"ack_delay_ms":1500,"upd_sentinel_key":"keep","newer_key":2}"#).unwrap();
    let before = manifest(&sentinels);
    assert!(install::roll_back_installed(&f.destination(), &f.state, &PRODUCT).unwrap_err().0.contains("quit"));
    f.stop_all();
    assert_eq!(install::roll_back_installed(&f.destination(), &f.state, &PRODUCT).unwrap(), "0.1.0-alpha.1");
    let slots = Slots::for_destination(&f.destination());
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.1");
    assert_eq!(f.version_at(&slots.rollback), "0.1.0-alpha.2");
    assert_eq!(manifest(&sentinels), before, "no data deleted or rewritten");
    // The old build starts.
    let _ = std::fs::remove_file(readiness::path(&f.state));
    let installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.1", "1", PRODUCT.bundle_id), "0.1.0-alpha.2"));
    (installer.launch)(&f.destination()).unwrap();
    let mut started = false;
    for _ in 0..50 {
        let data = std::fs::read(readiness::path(&f.state)).ok();
        if let readiness::State::Ready(_) = readiness::evaluate(data.as_deref(), "0.1.0-alpha.1", &|pid| {
            readiness::runs(pid, &f.destination().join("Contents/MacOS/Heimdall"))
        }) {
            started = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(started, "the restored build reports ready");
}

#[test]
#[ignore = "VM: the real older build opens the newer archive read-only-compatibly or refuses clearly; its settings save keeps newer keys (tests/vm/upd_vm.sh t21)"]
fn upd_t21_vm_archive_schema_and_settings_save() {
    panic!("run tests/vm/upd_vm.sh t21 in the VM guest");
}

// UPD-T22: Launch at Login and TCC grants are per user session and bundle; only a VM guest can check them.
#[test]
#[ignore = "VM: login item and TCC grants survive the in-place swap, no tccutil reset (tests/vm/upd_vm.sh t22)"]
fn upd_t22_launch_at_login_and_permission_grants_survive_the_swap() {
    panic!("run tests/vm/upd_vm.sh t22 in the VM guest");
}

// Review regressions: a failed version's workers are stopped with it; a rollback after a tool died mid-wait finds
// the previous version; another user's process counts as alive.
#[test]
fn a_failed_versions_workers_are_stopped_with_it() {
    let f = Fixture::new();
    f.install_old("0.1.0-alpha.1");
    let worker = f.destination().join("Contents/MacOS/heimdall-speakers-worker");
    let mut child = Command::new(&worker).env("HEIMDALL_UPDATE_STATE", &f.state).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(install::running_pids(&worker), [child.id() as i32]);
    install::terminate_app(&f.destination(), &PRODUCT);
    assert!(child.wait().is_ok());
    assert!(install::running_pids(&worker).is_empty());
}

#[test]
fn rollback_after_the_tool_died_waiting_for_readiness_restores_the_previous_version() {
    let f = Fixture::new();
    f.install_old("0.1.0-alpha.1");
    let mut installer = f.installer(f.plan(staged_from(&f, "0.1.0-alpha.2", "1", PRODUCT.bundle_id), "0.1.0-alpha.1"));
    installer.fault = Fault::CrashAfterSwap;
    assert!(installer.run().is_err());
    assert_eq!(install::roll_back_installed(&f.destination(), &f.state, &PRODUCT).unwrap(), "0.1.0-alpha.1");
    assert_eq!(f.version_at(&f.destination()), "0.1.0-alpha.1");
    let slots = Slots::for_destination(&f.destination());
    assert_eq!(f.version_at(&slots.rollback), "0.1.0-alpha.2");
    assert_eq!(f.txn_entries(), ["rollback"]);
}

#[test]
fn another_users_process_counts_as_alive() {
    assert!(readiness::alive(1), "launchd (root) answers EPERM, not ESRCH");
    assert!(!readiness::alive(0) && !readiness::alive(-1));
    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
    let pid = child.id() as i32;
    child.wait().unwrap();
    assert!(!readiness::alive(pid));
}
