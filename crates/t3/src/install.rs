//! T3 Code on this Mac: finding it, telling whether it speaks Orchestrator V2 (the MCP surface Heimdall drives:
//! `delegate_task`, `orchestrator_capabilities`, the thread tools), and installing the newest public release.
//!
//! Source: T3 Code's own update feed, the one its built-in updater reads (`Contents/Resources/app-update.yml`:
//! GitHub `pingdotgg/t3code`). `releases/latest` is the newest stable release; its `latest-mac.yml` names the zip
//! for each architecture with its size and SHA-512. The install downloads over HTTPS only (redirects too), checks
//! size and SHA-512, unpacks with `ditto`, checks the bundle ID and version, then `codesign --verify --deep
//! --strict` against T3 Tools' Developer ID (Team ID `ARK85ZXQ4Z`, identifier `com.t3tools.t3code`, Apple anchor),
//! and copies the app into `~/Applications` (no admin). Nothing is downloaded with a quarantine flag (curl sets
//! none), so no attribute is ever stripped. An app already at the destination is never replaced: T3 Code updates
//! itself.
//!
//! Orchestrator V2 first shipped in nightly 0.0.46 (3 Oct 2026) and is stable from 0.1.0 (15 Oct 2026).

use sha2::{Digest, Sha512};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const BUNDLE_ID: &str = "com.t3tools.t3code";
pub const TEAM_ID: &str = "ARK85ZXQ4Z";
/// The newest stable release's macOS update feed.
pub const FEED: &str = "https://github.com/pingdotgg/t3code/releases/latest/download/latest-mac.yml";
const RELEASES: &str = "https://github.com/pingdotgg/t3code/releases/download";
/// Where a person downloads T3 Code by hand.
pub const DOWNLOAD_PAGE: &str = "https://github.com/pingdotgg/t3code/releases/latest";
/// The first version with Orchestrator V2 (its nightlies; stable from 0.1.0).
pub const MIN_V2: (u64, u64, u64) = (0, 0, 46);
/// The code requirement an installed download must satisfy: T3 Tools' Developer ID, T3 Code's identifier.
pub fn official_requirement() -> String {
    format!("anchor apple generic and identifier \"{BUNDLE_ID}\" and certificate leaf[subject.OU] = \"{TEAM_ID}\"")
}

/// One T3 Code app bundle found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundApp {
    pub path: PathBuf,
    /// "T3 Code (Nightly)".
    pub name: String,
    pub version: String,
}

impl FoundApp {
    pub fn supports_v2(&self) -> bool {
        supports_v2(&self.version)
    }
}

/// What the wizard knows about T3 Code without talking to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detection {
    pub apps: Vec<FoundApp>,
    /// `~/.t3/bin/t3`, written by T3 Code on its first start.
    pub cli: bool,
    /// `~/.t3/userdata/server-runtime.json`: T3 Code's server has run.
    pub runtime: bool,
}

impl Detection {
    /// The app to show: the newest that supports V2, else the newest.
    pub fn best(&self) -> Option<&FoundApp> {
        let newest = |v2: bool| self.apps.iter().filter(|a| !v2 || a.supports_v2()).max_by_key(|a| version_key(&a.version));
        newest(true).or_else(|| newest(false))
    }
    pub fn ready(&self) -> bool {
        self.best().is_some_and(FoundApp::supports_v2)
    }
}

/// `/Applications` and `~/Applications`, where the wizard looks.
pub fn app_dirs(user_home: &Path) -> Vec<PathBuf> {
    vec![PathBuf::from("/Applications"), user_home.join("Applications")]
}

/// Looks for T3 Code bundles (any `.app` whose Info.plist names T3 Code's bundle ID) and its CLI and runtime file.
/// Reads only; never starts or contacts T3 Code.
pub fn detect(user_home: &Path, dirs: &[PathBuf]) -> Detection {
    let mut apps = vec![];
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "app") {
                continue;
            }
            if let Some(found) = read_app(&path) {
                apps.push(found);
            }
        }
    }
    apps.sort_by(|a, b| a.path.cmp(&b.path));
    let t3 = user_home.join(".t3");
    Detection { apps, cli: t3.join("bin/t3").is_file(), runtime: t3.join("userdata/server-runtime.json").is_file() }
}

fn read_app(path: &Path) -> Option<FoundApp> {
    let plist = fs::read_to_string(path.join("Contents/Info.plist")).ok()?;
    if plist_string(&plist, "CFBundleIdentifier")? != BUNDLE_ID {
        return None;
    }
    let version = plist_string(&plist, "CFBundleShortVersionString").or_else(|| plist_string(&plist, "CFBundleVersion"))?;
    let name = plist_string(&plist, "CFBundleName")
        .unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    Some(FoundApp { path: path.to_path_buf(), name, version })
}

fn plist_string(plist: &str, key: &str) -> Option<String> {
    let after = &plist[plist.find(&format!("<key>{key}</key>"))?..];
    let start = after.find("<string>")? + "<string>".len();
    let end = after[start..].find("</string>")?;
    Some(after[start..start + end].trim().to_string())
}

/// "0.1.3-nightly.20261025.3655" → (0, 1, 3).
fn core(version: &str) -> Option<(u64, u64, u64)> {
    let main = version.trim().trim_start_matches('v').split(['-', '+']).next()?;
    let mut parts = main.split('.').map(|p| p.parse::<u64>().ok());
    Some((parts.next()??, parts.next().flatten().unwrap_or(0), parts.next().flatten().unwrap_or(0)))
}

/// Orders versions by their numbers; a prerelease sorts below its release.
fn version_key(version: &str) -> ((u64, u64, u64), bool) {
    (core(version).unwrap_or((0, 0, 0)), !version.contains('-'))
}

/// Whether this T3 Code version has Orchestrator V2. Every build of 0.0.46 and later does (its first nightly
/// introduced it; there was no stable 0.0.46).
pub fn supports_v2(version: &str) -> bool {
    core(version).is_some_and(|c| c >= MIN_V2)
}

/// The release a feed offers for this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    /// The zip's file name, as the feed lists it.
    pub file: String,
    /// Base64 SHA-512 of the zip.
    pub sha512: String,
    pub size: u64,
}

impl Release {
    pub fn url(&self) -> String {
        format!("{RELEASES}/v{}/{}", self.version, self.file)
    }
}

/// `arm64` or `x64`, as T3 Code's release files are named.
pub fn arch() -> &'static str {
    if cfg!(target_arch = "aarch64") { "arm64" } else { "x64" }
}

/// Reads electron-builder's `latest-mac.yml`: the version, and the zip for `arch` with its SHA-512 and size.
pub fn parse_feed(text: &str, arch: &str) -> Result<Release, String> {
    let unquote = |v: &str| v.trim().trim_matches(['\'', '"']).to_string();
    let mut version = None;
    let mut files: Vec<(String, Option<String>, Option<u64>)> = vec![];
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("version:") {
            version = Some(unquote(v));
        } else if let Some(u) = t.strip_prefix("- url:") {
            files.push((unquote(u), None, None));
        } else if let Some(h) = t.strip_prefix("sha512:") {
            if line.starts_with(' ') {
                if let Some(f) = files.last_mut() { f.1 = Some(unquote(h)); }
            }
        } else if let Some(s) = t.strip_prefix("size:") {
            if let Some(f) = files.last_mut() { f.2 = unquote(s).parse().ok(); }
        }
    }
    let version = version.filter(|v| core(v).is_some()).ok_or("The T3 Code feed names no version.")?;
    let (file, sha512, size) = files
        .into_iter()
        .find(|(u, _, _)| u.ends_with(".zip") && u.contains(&format!("-{arch}.")))
        .ok_or_else(|| format!("T3 Code {version} has no download for this Mac."))?;
    if file.contains('/') || file.contains("..") {
        return Err("The T3 Code feed names an unexpected file.".into());
    }
    Ok(Release { version, file, sha512: sha512.ok_or("The T3 Code feed has no checksum.")?, size: size.ok_or("The T3 Code feed has no size.")? })
}

/// Downloads a URL to a file.
pub trait Fetch {
    fn get(&self, url: &str, to: &Path) -> Result<(), String>;
}

/// HTTPS only, redirects included (GitHub sends release files from its own storage host).
pub struct Https;

impl Fetch for Https {
    fn get(&self, url: &str, to: &Path) -> Result<(), String> {
        if !url.starts_with("https://") {
            return Err("Only HTTPS downloads are allowed.".into());
        }
        let status = Command::new("/usr/bin/curl")
            .args(["--fail", "--silent", "--show-error", "--location", "--max-redirs", "5"])
            .args(["--proto", "=https", "--proto-redir", "=https", "--tlsv1.2", "--connect-timeout", "15"])
            .arg("--output").arg(to).arg(url)
            .status()
            .map_err(|e| format!("curl did not start: {e}"))?;
        if status.success() { Ok(()) } else { Err(format!("Downloading {url} failed.")) }
    }
}

/// Where an install is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Checking,
    Downloading,
    Verifying,
    Copying,
}

/// Installs the newest public T3 Code release into `dest_dir` (normally `~/Applications`). `requirement` is the
/// code requirement the app must satisfy (`official_requirement()`; tests pass their own). Returns the installed
/// app. Every refusal leaves `dest_dir` untouched and removes the download.
pub fn install(fetch: &dyn Fetch, feed: &str, arch: &str, dest_dir: &Path, requirement: &str, progress: &dyn Fn(Stage)) -> Result<PathBuf, String> {
    let work = std::env::temp_dir().join(format!(
        "heimdall-t3-install-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
    ));
    fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let result = install_in(fetch, feed, arch, dest_dir, requirement, progress, &work);
    let _ = fs::remove_dir_all(&work);
    result
}

fn install_in(fetch: &dyn Fetch, feed: &str, arch: &str, dest_dir: &Path, requirement: &str, progress: &dyn Fn(Stage), work: &Path) -> Result<PathBuf, String> {
    progress(Stage::Checking);
    let feed_file = work.join("latest-mac.yml");
    fetch.get(feed, &feed_file)?;
    let text = fs::read_to_string(&feed_file).map_err(|e| format!("The T3 Code feed is unreadable: {e}"))?;
    let release = parse_feed(&text, arch)?;
    if !supports_v2(&release.version) {
        return Err(format!("T3 Code {} is too old for Heimdall.", release.version));
    }
    progress(Stage::Downloading);
    let zip = work.join(&release.file);
    fetch.get(&release.url(), &zip)?;
    progress(Stage::Verifying);
    check_download(&zip, &release)?;
    let unpacked = work.join("unpacked");
    fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let ok = Command::new("/usr/bin/ditto").arg("-x").arg("-k").arg(&zip).arg(&unpacked).status().map_err(|e| e.to_string())?;
    if !ok.success() {
        return Err("The T3 Code download would not unpack.".into());
    }
    let apps: Vec<PathBuf> = fs::read_dir(&unpacked)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .collect();
    let [app] = apps.as_slice() else { return Err("The T3 Code download holds no single app.".into()) };
    let found = read_app(app).ok_or("The download is not T3 Code.")?;
    if found.version != release.version {
        return Err(format!("The download is T3 Code {}, not {}.", found.version, release.version));
    }
    verify_signature(app, requirement)?;
    progress(Stage::Copying);
    let name = app.file_name().ok_or("The app has no name.")?;
    let dest = dest_dir.join(name);
    if fs::symlink_metadata(&dest).is_ok() {
        return Err(format!("{} is already there; it updates itself.", dest.display()));
    }
    fs::create_dir_all(dest_dir).map_err(|e| format!("{}: {e}", dest_dir.display()))?;
    let partial = dest_dir.join(format!(".{}.partial", name.to_string_lossy()));
    let _ = fs::remove_dir_all(&partial);
    let ok = Command::new("/usr/bin/ditto").arg(app).arg(&partial).status().map_err(|e| e.to_string())?;
    if !ok.success() {
        let _ = fs::remove_dir_all(&partial);
        return Err(format!("Copying T3 Code into {} failed.", dest_dir.display()));
    }
    fs::rename(&partial, &dest).map_err(|e| {
        let _ = fs::remove_dir_all(&partial);
        format!("{}: {e}", dest.display())
    })?;
    Ok(dest)
}

/// The zip's size and SHA-512 against the feed.
pub fn check_download(zip: &Path, release: &Release) -> Result<(), String> {
    use base64::Engine as _;
    let bytes = fs::read(zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    if bytes.len() as u64 != release.size {
        return Err(format!("The T3 Code download is {} bytes, not {}.", bytes.len(), release.size));
    }
    let digest = base64::engine::general_purpose::STANDARD.encode(Sha512::digest(&bytes));
    if digest != release.sha512 {
        return Err("The T3 Code download does not match its checksum.".into());
    }
    Ok(())
}

/// `codesign --verify --deep --strict` with `requirement` as the test requirement.
pub fn verify_signature(app: &Path, requirement: &str) -> Result<(), String> {
    let out = Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R"])
        .arg(format!("={requirement}"))
        .arg(app)
        .output()
        .map_err(|e| format!("codesign did not start: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let why = String::from_utf8_lossy(&out.stderr);
        Err(format!("T3 Code's signature did not check out: {}", why.lines().last().unwrap_or("").trim()))
    }
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
