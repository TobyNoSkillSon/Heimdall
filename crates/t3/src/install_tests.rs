//! T3 Code detection and install against fixture folders. Nothing here touches ~/.t3, /Applications or the network.
use super::*;
use base64::Engine as _;
use std::cell::RefCell;

fn plist(id: &str, name: &str, version: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
        <key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>{name}</string>\
        <key>CFBundleExecutable</key><string>t3</string>\
        <key>CFBundleShortVersionString</key><string>{version}</string></dict></plist>")
}

fn fake_app(dir: &Path, file: &str, id: &str, version: &str) -> PathBuf {
    let app = dir.join(file);
    fs::create_dir_all(app.join("Contents/MacOS")).unwrap();
    fs::write(app.join("Contents/Info.plist"), plist(id, file.trim_end_matches(".app"), version)).unwrap();
    let exe = app.join("Contents/MacOS/t3");
    fs::write(&exe, "#!/bin/sh\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    app
}

#[test]
fn t3_versions_with_orchestrator_v2() {
    assert!(!supports_v2("0.0.45"));
    assert!(supports_v2("0.0.46-nightly.20261003.2610"));
    assert!(supports_v2("0.1.0"));
    assert!(supports_v2("0.1.3-nightly.20261025.3655"));
    assert!(supports_v2("v1.0"));
    assert!(!supports_v2("garbage"));
}

#[test]
fn t3_detection_states_from_fixture_folders() {
    let home = tempfile::tempdir().unwrap();
    let system = tempfile::tempdir().unwrap();
    let user_apps = home.path().join("Applications");
    let dirs = vec![system.path().to_path_buf(), user_apps.clone()];
    // Missing: nothing anywhere.
    let none = detect(home.path(), &dirs);
    assert_eq!((none.best(), none.ready(), none.cli, none.runtime), (None, false, false, false));
    // Too old: only an Alpha 0.0.45; another app's bundle is ignored.
    fake_app(system.path(), "T3 Code (Alpha).app", BUNDLE_ID, "0.0.45");
    fake_app(system.path(), "Other.app", "com.example.other", "9.9.9");
    let old = detect(home.path(), &dirs);
    assert_eq!(old.apps.len(), 1);
    assert_eq!(old.best().unwrap().version, "0.0.45");
    assert!(!old.ready());
    // Found and ready: a nightly in ~/Applications wins over the old one; the CLI and runtime file are seen.
    fake_app(&user_apps, "T3 Code (Nightly).app", BUNDLE_ID, "0.1.3-nightly.20261025.3655");
    fs::create_dir_all(home.path().join(".t3/bin")).unwrap();
    fs::write(home.path().join(".t3/bin/t3"), "").unwrap();
    fs::create_dir_all(home.path().join(".t3/userdata")).unwrap();
    fs::write(home.path().join(".t3/userdata/server-runtime.json"), "{}").unwrap();
    let found = detect(home.path(), &dirs);
    assert!(found.ready() && found.cli && found.runtime);
    assert_eq!(found.best().unwrap().name, "T3 Code (Nightly)");
}

const FEED_TEXT: &str = "version: '0.1.3'\nfiles:\n  - url: T3-Code-0.1.3-arm64.zip\n    sha512: AAA=\n    size: 10\n  - url: T3-Code-0.1.3-arm64.dmg\n    sha512: BBB=\n    size: 20\n  - url: T3-Code-0.1.3-x64.zip\n    sha512: CCC=\n    size: 30\nreleaseDate: '2026-10-27T06:07:04.997Z'\n";

#[test]
fn t3_feed_names_the_zip_for_this_mac() {
    let arm = parse_feed(FEED_TEXT, "arm64").unwrap();
    assert_eq!(arm, Release { version: "0.1.3".into(), file: "T3-Code-0.1.3-arm64.zip".into(), sha512: "AAA=".into(), size: 10 });
    assert_eq!(arm.url(), "https://github.com/pingdotgg/t3code/releases/download/v0.1.3/T3-Code-0.1.3-arm64.zip");
    assert_eq!(parse_feed(FEED_TEXT, "x64").unwrap().sha512, "CCC=");
    assert!(parse_feed("files:\n", "arm64").is_err());
    assert!(parse_feed(&FEED_TEXT.replace("T3-Code-0.1.3-arm64.zip", "../evil-arm64.zip"), "arm64").is_err());
}

/// Serves files from a fixture folder by the URL's last segment, and records what was asked for.
struct Fixture {
    dir: PathBuf,
    asked: RefCell<Vec<String>>,
}
impl Fetch for Fixture {
    fn get(&self, url: &str, to: &Path) -> Result<(), String> {
        self.asked.borrow_mut().push(url.to_string());
        let name = url.rsplit('/').next().unwrap();
        fs::copy(self.dir.join(name), to).map(|_| ()).map_err(|e| e.to_string())
    }
}

/// A release folder: the zipped app (signed ad hoc with T3's identifier when `sign`) and a feed naming it.
fn release(version: &str, sign: bool, corrupt_hash: bool) -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let app = fake_app(&stage, "T3 Code.app", BUNDLE_ID, version);
    if sign {
        assert!(Command::new("/usr/bin/codesign").args(["-s", "-", "-i", BUNDLE_ID]).arg(&app).status().unwrap().success());
    }
    let file = format!("T3-Code-{version}-arm64.zip");
    let zip = dir.path().join(&file);
    assert!(Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent"]).arg(&app).arg(&zip).status().unwrap().success());
    let bytes = fs::read(&zip).unwrap();
    let mut hash = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes));
    if corrupt_hash {
        hash = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(b"something else"));
    }
    fs::write(dir.path().join("latest-mac.yml"), format!("version: '{version}'\nfiles:\n  - url: {file}\n    sha512: {hash}\n    size: {}\n", bytes.len())).unwrap();
    let fixture = Fixture { dir: dir.path().to_path_buf(), asked: RefCell::new(vec![]) };
    (dir, fixture)
}

const AD_HOC: &str = "identifier \"com.t3tools.t3code\"";
const FEED_URL: &str = "https://example.invalid/latest-mac.yml";

#[test]
fn t3_install_copies_a_verified_release_and_never_replaces() {
    let (_dir, fixture) = release("0.1.3", true, false);
    let dest = tempfile::tempdir().unwrap();
    let stages = RefCell::new(vec![]);
    let installed = install(&fixture, FEED_URL, "arm64", dest.path(), AD_HOC, &|s| stages.borrow_mut().push(s)).unwrap();
    assert_eq!(installed, dest.path().join("T3 Code.app"));
    assert_eq!(read_app(&installed).unwrap().version, "0.1.3");
    assert_eq!(*stages.borrow(), [Stage::Checking, Stage::Downloading, Stage::Verifying, Stage::Copying]);
    assert_eq!(fixture.asked.borrow()[1], "https://github.com/pingdotgg/t3code/releases/download/v0.1.3/T3-Code-0.1.3-arm64.zip");
    // A second install never replaces the app that is there.
    let again = install(&fixture, FEED_URL, "arm64", dest.path(), AD_HOC, &|_| {});
    assert!(again.unwrap_err().contains("already there"));
}

#[test]
fn t3_install_refuses_a_bad_hash() {
    let (_dir, fixture) = release("0.1.3", true, true);
    let dest = tempfile::tempdir().unwrap();
    let err = install(&fixture, FEED_URL, "arm64", dest.path(), AD_HOC, &|_| {}).unwrap_err();
    assert!(err.contains("checksum"), "{err}");
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn t3_install_refuses_a_bad_signature() {
    // Unsigned.
    let (_dir, fixture) = release("0.1.3", false, false);
    let dest = tempfile::tempdir().unwrap();
    let err = install(&fixture, FEED_URL, "arm64", dest.path(), AD_HOC, &|_| {}).unwrap_err();
    assert!(err.contains("signature"), "{err}");
    // Signed, but not by T3 Tools' Developer ID: the official requirement refuses it.
    let (_dir, fixture) = release("0.1.3", true, false);
    let err = install(&fixture, FEED_URL, "arm64", dest.path(), &official_requirement(), &|_| {}).unwrap_err();
    assert!(err.contains("signature"), "{err}");
    assert_eq!(fs::read_dir(dest.path()).unwrap().count(), 0);
}

#[test]
fn t3_install_refuses_a_release_without_v2() {
    let (_dir, fixture) = release("0.0.45", true, false);
    let dest = tempfile::tempdir().unwrap();
    let err = install(&fixture, FEED_URL, "arm64", dest.path(), AD_HOC, &|_| {}).unwrap_err();
    assert!(err.contains("too old"), "{err}");
    assert_eq!(fixture.asked.borrow().len(), 1, "nothing downloaded past the feed");
}

#[test]
fn t3_https_fetch_refuses_plain_http() {
    let dir = tempfile::tempdir().unwrap();
    assert!(Https.get("http://example.com/x", &dir.path().join("x")).is_err());
}
