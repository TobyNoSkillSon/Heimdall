//! Local fixture releases in a temp dir: fake `Heimdall Alpha.app` bundles (a tiny compiled program as every
//! executable, ad-hoc signed), zips made with ditto, SHA256SUMS signed with a key generated per test, and an isolated
//! profile (Applications, state and data folders) under the temp dir. Nothing outside the temp dir is touched; fixture
//! apps are started directly, never through LaunchServices.

use crate::bundle::IdentityCheck;
use crate::fetch::HttpFetch;
use crate::install::{Clock, Installer, Plan};
use crate::prepare::{Prepare, Staged};
use crate::release::{SUMS, SUMS_SIG};
use crate::source::Source;
use crate::{signing, Product, Release, Version};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub const PRODUCT: Product = Product::ALPHA;

const PROGRAM: &str = r#"
#include <mach-o/dyld.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
int main(void) {
  char exe[4096]; uint32_t n = sizeof exe;
  if (_NSGetExecutablePath(exe, &n)) return 2;
  char *slash = strrchr(exe, '/'); if (slash) *slash = 0;
  char modep[4300]; snprintf(modep, sizeof modep, "%s/../Resources/fixture-mode", exe);
  char mode[64] = "ready", version[64] = "0.0.0";
  FILE *f = fopen(modep, "r"); if (f) { if (fscanf(f, "%63s %63s", mode, version) != 2) return 4; fclose(f); }
  if (!strcmp(mode, "exit1")) return 1;
  const char *state = getenv("HEIMDALL_UPDATE_STATE"); if (!state) return 3;
  char path[4300], tmp[4400];
  snprintf(path, sizeof path, "%s/ready.json", state);
  snprintf(tmp, sizeof tmp, "%s.%d.tmp", path, getpid());
  const char *phase = !strcmp(mode, "failing") ? "failing" : !strcmp(mode, "degraded") ? "degraded" : "ready";
  FILE *o = fopen(tmp, "w"); if (!o) return 5;
  fprintf(o, "{\"app_pid\":%d,\"version\":\"%s\",\"phase\":\"%s\",\"detail\":\"fixture\"}", getpid(), version, phase);
  fclose(o); rename(tmp, path);
  sleep(60);
  return 0;
}
"#;

pub struct Fixture {
    _dir: tempfile::TempDir,
    pub root: PathBuf,
    pub apps: PathBuf,
    pub state: PathBuf,
    pub data: PathBuf,
    pub work: PathBuf,
    pub releases: PathBuf,
    pub seed: [u8; 32],
    program: PathBuf,
    pub pids: Arc<Mutex<Vec<i32>>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for &pid in self.pids.lock().unwrap().iter() {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

pub fn run(cmd: &mut Command) {
    let out = cmd.output().expect("tool runs");
    assert!(out.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&out.stderr));
}

impl Fixture {
    pub fn new() -> Fixture {
        let dir = tempfile::Builder::new().prefix("upd-home").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let program = root.join("fixture-bin");
        let source = root.join("fixture.c");
        std::fs::write(&source, PROGRAM).unwrap();
        run(Command::new("/usr/bin/clang").args(["-O1", "-o"]).arg(&program).arg(&source));
        let f = Fixture {
            apps: root.join("Applications"),
            state: root.join("Library/Application Support/com.tobynoskillson.heimdall/updates"),
            data: root.join("home/.heimdall"),
            work: root.join("tmp"),
            releases: root.join("releases"),
            seed: signing::generate_seed().unwrap(),
            program,
            pids: Arc::new(Mutex::new(vec![])),
            root,
            _dir: dir,
        };
        for d in [&f.apps, &f.state, &f.data, &f.work, &f.releases] {
            std::fs::create_dir_all(d).unwrap();
        }
        f
    }

    pub fn destination(&self) -> PathBuf {
        self.apps.join(PRODUCT.app_folder)
    }

    pub fn keys(&self) -> Vec<[u8; 32]> {
        vec![signing::public_key(&self.seed)]
    }

    /// A fake app at `parent/Heimdall Alpha.app` with the product's bundle ID, ad-hoc signed with the signing
    /// identifier `identifier`. `mode`: ready, exit1, failing, degraded.
    pub fn make_app(&self, parent: &Path, version: &str, build: &str, mode: &str, identifier: &str) -> PathBuf {
        let app = parent.join(PRODUCT.app_folder);
        let _ = std::fs::remove_dir_all(&app);
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        for file in PRODUCT.required {
            let path = contents.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::copy(&self.program, &path).unwrap();
        }
        std::fs::write(contents.join("Resources/fixture-mode"), format!("{mode} {version}\n")).unwrap();
        let mut dict = plist::Dictionary::new();
        for (k, v) in [
            ("CFBundleIdentifier", PRODUCT.bundle_id),
            ("CFBundleName", "Heimdall Alpha"),
            ("CFBundleExecutable", "Heimdall"),
            ("CFBundlePackageType", "APPL"),
            ("CFBundleShortVersionString", version),
            ("CFBundleVersion", build),
            ("LSMinimumSystemVersion", "14.4"),
        ] {
            dict.insert(k.into(), plist::Value::String(v.into()));
        }
        plist::Value::Dictionary(dict).to_file_xml(contents.join("Info.plist")).unwrap();
        run(Command::new("/usr/bin/codesign").args(["--force", "--deep", "--sign", "-", "--identifier", identifier]).arg(&app));
        app
    }

    pub fn install_old(&self, version: &str) -> PathBuf {
        self.make_app(&self.apps, version, "1", "ready", PRODUCT.bundle_id)
    }

    /// Packages `app` as a release in `releases/v<version>/` with signed SHA256SUMS and lists it in releases.json.
    pub fn publish(&self, app: &Path, version: &str) -> Release {
        let tag = format!("v{version}");
        let dir = self.releases.join(&tag);
        std::fs::create_dir_all(&dir).unwrap();
        let v = Version::parse(version).unwrap();
        let zip = dir.join(PRODUCT.zip_name(&v));
        run(Command::new("/usr/bin/ditto")
            .args(["-c", "-k", "--norsrc", "--noextattr", "--noqtn", "--keepParent"])
            .arg(app)
            .arg(&zip));
        self.sign_sums(&dir, &[&zip]);
        let release = Release {
            tag: tag.clone(),
            version: v.clone(),
            name: format!("Heimdall Alpha {version}"),
            body: "Fixture release.".into(),
            draft: false,
            prerelease: v.is_prerelease(),
            published: true,
            assets: vec![PRODUCT.zip_name(&v), SUMS.into(), SUMS_SIG.into()],
        };
        let list = serde_json::json!([{
            "tag_name": tag, "name": release.name, "body": release.body, "draft": false,
            "prerelease": release.prerelease, "published_at": "2026-10-27T00:00:00Z",
            "assets": release.assets.iter().map(|a| serde_json::json!({"name": a})).collect::<Vec<_>>(),
        }]);
        std::fs::write(self.releases.join("releases.json"), serde_json::to_vec(&list).unwrap()).unwrap();
        release
    }

    /// Writes SHA256SUMS for `files` (by name) into `dir` and signs it with the fixture key.
    pub fn sign_sums(&self, dir: &Path, files: &[&Path]) {
        let mut sums = String::new();
        for file in files {
            sums += &format!("{}  {}\n", crate::sha256_file(file).unwrap(), file.file_name().unwrap().to_string_lossy());
        }
        std::fs::write(dir.join(SUMS), &sums).unwrap();
        std::fs::write(dir.join(SUMS_SIG), signing::sign(&self.seed, sums.as_bytes())).unwrap();
    }

    pub fn source(&self) -> Source {
        Source::from_base(Some(&format!("file://{}", self.releases.display()))).unwrap()
    }

    pub fn prepare(&self, release: &Release) -> crate::Result<Staged> {
        self.prepare_with(release, &HttpFetch::new().unwrap(), self.source())
    }

    pub fn prepare_with(&self, release: &Release, fetch: &dyn crate::fetch::Fetch, source: Source) -> crate::Result<Staged> {
        let keys = self.keys();
        let prepare = Prepare {
            product: PRODUCT,
            source,
            fetch,
            keys: &keys,
            identity: IdentityCheck::new(&PRODUCT),
            running_app: self.destination(),
            work_root: self.work.clone(),
        };
        prepare.run(release, &mut |_| {})
    }

    pub fn plan(&self, staged: Staged, from: &str) -> Plan {
        Plan { staged, destination: self.destination(), from: from.into(), wait_for_pid: None, state_dir: self.state.clone(), allow_downgrade: false }
    }

    /// An installer with the real swap and signature checks, launching fixture apps directly (never through
    /// LaunchServices) with the isolated state folder, short timeouts.
    pub fn installer(&self, plan: Plan) -> Installer {
        let mut installer = Installer::new(plan, PRODUCT);
        let state = self.state.clone();
        let pids = self.pids.clone();
        installer.launch = Box::new(move |app| {
            let exe = app.join("Contents/MacOS/Heimdall");
            let mut child = Command::new(exe).env("HEIMDALL_UPDATE_STATE", &state).spawn()?;
            pids.lock().unwrap().push(child.id() as i32);
            std::thread::spawn(move || child.wait());
            Ok(())
        });
        installer.start_timeout = 5.0;
        installer.settle = 0.5;
        installer.poll = 0.05;
        installer
    }

    /// Stops every fixture process running from the destination (the app quit before a hand-off).
    pub fn stop_all(&self) {
        for _ in 0..50 {
            let pids = self.running();
            if pids.is_empty() {
                return;
            }
            for pid in pids {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        panic!("fixture processes did not stop");
    }

    /// Pids running the installed app's executable.
    pub fn running(&self) -> Vec<i32> {
        crate::install::running_pids(&self.destination().join("Contents/MacOS/Heimdall"))
    }

    pub fn version_at(&self, app: &Path) -> String {
        crate::bundle::info(app).unwrap().version
    }

    pub fn txn_entries(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.apps.join(crate::install::TXN))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    pub fn work_is_empty(&self) -> bool {
        std::fs::read_dir(&self.work).unwrap().next().is_none()
    }
}

/// path -> (sha256, size, mtime) for every sentinel.
pub type Manifest = BTreeMap<PathBuf, (String, u64, i64)>;

pub fn manifest(paths: &[PathBuf]) -> Manifest {
    use std::os::unix::fs::MetadataExt;
    paths
        .iter()
        .map(|p| {
            let meta = std::fs::metadata(p).unwrap();
            (p.clone(), (crate::sha256_file(p).unwrap(), meta.len(), meta.mtime()))
        })
        .collect()
}

/// Plants the file sentinels of UPD-T02, T04 and T06 under the isolated profile; returns their paths.
pub fn plant_sentinels(f: &Fixture) -> Vec<PathBuf> {
    let files = [
        (f.data.join("models/UPD-SENTINEL/weights.bin"), b"model-weights-sentinel".to_vec()),
        (f.data.join("splash/prefix-cache/UPD-SENTINEL"), b"prefix-cache-sentinel".to_vec()),
        (f.data.join("splash/engine.json"), br#"{"model":"upd-sentinel"}"#.to_vec()),
        (f.root.join("hf-cache/models--upd--sentinel/blobs/UPD-SENTINEL"), b"hf-cache-sentinel".to_vec()),
        (f.data.join("settings.json"), br#"{"ack_delay_ms":1500,"upd_sentinel_key":"keep"}"#.to_vec()),
        (f.data.join("agent/skills/upd-custom/SKILL.md"), b"custom-v1".to_vec()),
    ];
    for (path, bytes) in &files {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    files.into_iter().map(|(p, _)| p).collect()
}

/// A clock that only moves when the code under test sleeps; `on_tick` runs at each sleep with the new time.
pub struct FakeClock {
    pub now: std::cell::Cell<f64>,
    pub on_tick: Box<dyn Fn(f64)>,
}

impl Clock for FakeClock {
    fn now(&self) -> f64 {
        self.now.get()
    }
    fn sleep(&self, seconds: f64) {
        self.now.set(self.now.get() + seconds);
        (self.on_tick)(self.now.get());
    }
}
