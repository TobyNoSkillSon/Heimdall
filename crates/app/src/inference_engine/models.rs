//! The engine's models on this Mac: which precisions are installed, downloading one, deleting one.
//!
//! Splash's layout (crates/splash/src/hub): `heimdall-splash pull OWNER/REPO:VARIANT --models-dir <models root>`
//! (server/cli.rs `pull`) downloads the weights into the Hugging Face hub cache (`models--o--r/{blobs,snapshots,refs}`,
//! hub/cache.rs `download_one`), builds an assembly `<models root>/.resolved/<sha256>` of links to the snapshot files
//! and to derived metadata in `<models root>/.metadata/<key>` (hub/assembly.rs `build`, `derived_metadata`), pins each
//! snapshot under `refs/splash/<sha256 of the selection link>/<commit>` (hub/cache.rs `pin`) and points the selection
//! link `<models root>/OWNER/REPO:VARIANT` at the assembly (hub/mod.rs `selection_link`, assembly.rs
//! `link_selection`). Splash has no delete command; this module removes one precision the way its collection does
//! (assembly.rs `collect_garbage`, under `installation_lock`), restricted to that precision's own files.
use anyhow::{bail, ensure, Context, Result};
use crossbeam_channel::Sender;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read},
    os::unix::{ffi::OsStrExt, io::AsRawFd, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use super::check_source;

/// Where the engine's models live.
#[derive(Clone, Debug)]
pub struct Roots {
    /// Heimdall's assemblies and selection links: `<data>/splash/models` (the server's default beside its cache,
    /// crates/splash/src/server/cli.rs `models_dir`).
    pub models: PathBuf,
    /// The Hugging Face hub cache the weights live in (the installer runs with `HF_HUB_CACHE` set to it).
    pub hf: PathBuf,
    /// Other Splash models roots whose installations may link the same cache files (Homebrew Splash's). Read only.
    pub others: Vec<PathBuf>,
    /// What Heimdall's own pulls downloaded, one manifest per installation (`<data>/splash/downloads`): the only
    /// hub-cache files a delete may remove.
    pub manifests: PathBuf,
}
impl Roots {
    /// The app's roots. Test builds never see the real caches: everything is under `data`.
    pub fn for_data(data: &Path) -> Self {
        if cfg!(test) {
            return Self { models: data.join("splash/models"), hf: data.join("hf-cache/hub"), others: vec![], manifests: data.join("splash/downloads") };
        }
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        Self {
            models: data.join("splash/models"),
            hf: hf_cache(),
            others: vec![home.join("Library/Application Support/Splash/models")],
            manifests: data.join("splash/downloads"),
        }
    }
}

/// HF_HUB_CACHE, else HUGGINGFACE_HUB_CACHE, else HF_HOME/hub, else $XDG_CACHE_HOME/huggingface/hub, else
/// ~/.cache/huggingface/hub: where Splash's hub looks (crates/splash/src/hub/cache.rs `default_cache`).
pub fn hf_cache() -> PathBuf {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let expand = |v: String| v.strip_prefix("~/").map_or_else(|| PathBuf::from(&v), |rest| home.join(rest));
    if let Ok(v) = std::env::var("HF_HUB_CACHE").or_else(|_| std::env::var("HUGGINGFACE_HUB_CACHE")) {
        return expand(v);
    }
    if let Ok(v) = std::env::var("HF_HOME") {
        return expand(v).join("hub");
    }
    std::env::var("XDG_CACHE_HOME").map(expand).unwrap_or_else(|_| home.join(".cache")).join("huggingface/hub")
}

fn read_record(assembly: &Path) -> Option<Value> {
    serde_json::from_slice::<Value>(&fs::read(assembly.join("model.json")).ok()?).ok().filter(|v| v["files"].is_object())
}
fn safe_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('/') && name.split('/').all(|p| !p.is_empty() && p != "." && p != "..")
}

/// Whether the precision `source` is installed under `models`: its selection link resolves to an assembly whose
/// model.json lists the target weights, and every file it lists is there with its recorded size. File checks only:
/// no process, no hashing (a full content check is Splash's own, at the engine's start).
pub fn installed(models: &Path, source: &str) -> bool {
    if check_source(source).is_err() {
        return false;
    }
    let link = models.join(source);
    if !fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    let Ok(assembly) = link.canonicalize() else { return false };
    if assembly.parent().and_then(Path::file_name).is_none_or(|n| n != ".resolved") {
        return false;
    }
    let Some(record) = read_record(&assembly) else { return false };
    let files = record["files"].as_object().expect("checked");
    files.keys().any(|n| n.starts_with("target/"))
        && files.iter().all(|(name, entry)| {
            safe_name(name)
                && fs::metadata(assembly.join(name)).is_ok_and(|m| m.is_file() && Some(m.len()) == entry["bytes"].as_u64())
        })
}

// ---- download

/// What installs a precision. The app runs `heimdall-splash pull`; tests run a fake that prints progress lines.
#[derive(Clone, Debug)]
pub enum Installer {
    /// `<program> pull <source> --models-dir <models root>`, with `HF_HUB_CACHE` set to the hub cache.
    Program(PathBuf),
    /// No installer (the default in test builds, so no test can start a real download).
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DownloadEvent {
    Progress { id: String, quant: String, done: u64, total: Option<u64> },
    /// The installer ended: `Ok` installed, `Err` its last words, `canceled` when the user stopped it.
    Done { id: String, quant: String, result: std::result::Result<(), String>, canceled: bool },
}

/// One `Downloading · repo · file · N% of X GB` line (crates/splash/src/server/cli.rs `progress`, every 5% per file,
/// after a `HH:MM:SS ` time): the repository, the file, and its done and total bytes (derived from the percentage).
pub fn parse_progress(line: &str) -> Option<(String, String, u64, u64)> {
    let rest = &line[line.find("Downloading · ")? + "Downloading · ".len()..];
    let parts: Vec<&str> = rest.split(" · ").collect();
    let [repo, file, amount] = parts[..] else { return None };
    let (pct, total) = amount.trim().split_once("% of ")?;
    let pct: u64 = pct.trim().parse().ok()?;
    let gb: f64 = total.trim().strip_suffix("GB")?.trim().parse().ok()?;
    let total = (gb * 1e9) as u64;
    Some((repo.into(), file.into(), total * pct.min(100) / 100, total))
}

/// A running installer. Dropping it (Heimdall quitting) stops it and keeps its partial files, so the next download
/// resumes; `cancel` (the user's Cancel) also removes them.
pub struct Download {
    pub id: String,
    pub quant: String,
    pub source: String,
    cancel: Arc<AtomicBool>,
    discard: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
/// At most this often a progress update reaches the menu (four a second).
pub const PROGRESS_EVERY: Duration = Duration::from_millis(250);
const CANCEL_GRACE: Duration = Duration::from_secs(3);

impl Download {
    /// Start installing `source` at low priority in its own process group. `total_hint`: the catalog's size.
    pub fn start(
        installer: &Installer,
        roots: &Roots,
        id: &str,
        quant: &str,
        source: &str,
        total_hint: Option<u64>,
        events: Sender<DownloadEvent>,
    ) -> Result<Self> {
        check_source(source)?;
        let Installer::Program(program) = installer else { bail!("no model installer in this build") };
        crate::settings::private_dir(&roots.models)?;
        let mut command = Command::new(program);
        command
            .arg("pull")
            .arg(source)
            .arg("--models-dir")
            .arg(&roots.models)
            .env("HF_HUB_CACHE", &roots.hf)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                // Its own group, so Cancel stops whatever it starts; the lowest CPU priority while the user works.
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::setpriority(libc::PRIO_PROCESS, 0, 19);
                Ok(())
            });
        }
        let mut child = command.spawn().with_context(|| format!("start {}", program.display()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let discard = Arc::new(AtomicBool::new(false));
        let (id_, quant_, roots_, source_) = (id.to_string(), quant.to_string(), roots.clone(), source.to_string());
        let (stop, discard_partials) = (cancel.clone(), discard.clone());
        let stderr = child.stderr.take().context("installer output")?;
        let mut stdout = child.stdout.take().context("installer output")?;
        let thread = thread::spawn(move || {
            let pid = child.id() as i32;
            let (lines_tx, lines) = crossbeam_channel::unbounded::<String>();
            let reader = thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
                    if lines_tx.send(line).is_err() {
                        break;
                    }
                }
            });
            let drain = thread::spawn(move || {
                let mut s = String::new();
                let _ = stdout.read_to_string(&mut s);
            });
            let mut files: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            let mut repos = BTreeSet::new();
            // (repository, file) of every file Splash fetched: it prints progress only for files it downloads.
            let mut fetched: BTreeSet<(String, String)> = BTreeSet::new();
            let mut last_words = String::new();
            let mut sent: Option<Instant> = None;
            let mut pending = false;
            let mut canceled_at: Option<Instant> = None;
            let progress = |files: &BTreeMap<String, (u64, u64)>| {
                let done = files.values().map(|f| f.0).sum::<u64>();
                let seen = files.values().map(|f| f.1).sum::<u64>();
                DownloadEvent::Progress { id: id_.clone(), quant: quant_.clone(), done, total: total_hint.max(Some(seen)).filter(|t| *t > 0) }
            };
            let _ = events.send(progress(&files));
            let status = loop {
                match lines.recv_timeout(Duration::from_millis(50)) {
                    Ok(line) => {
                        if let Some((repo, file, done, total)) = parse_progress(&line) {
                            files.insert(format!("{repo}/{file}"), (done, total));
                            fetched.insert((repo.clone(), file));
                            repos.insert(repo);
                            pending = true;
                        } else if !line.trim().is_empty() {
                            last_words = line;
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        if let Ok(Some(status)) = child.try_wait() {
                            break status;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                }
                if pending && sent.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) {
                    let _ = events.send(progress(&files));
                    sent = Some(Instant::now());
                    pending = false;
                }
                // The child is not reaped yet, so its group id is still ours to signal.
                if stop.load(Ordering::Acquire) {
                    match canceled_at {
                        None => {
                            unsafe { libc::kill(-pid, libc::SIGTERM) };
                            canceled_at = Some(Instant::now());
                        }
                        Some(t) if t.elapsed() >= CANCEL_GRACE => unsafe {
                            libc::kill(-pid, libc::SIGKILL);
                        },
                        _ => {}
                    }
                }
                if let Ok(Some(status)) = child.try_wait() {
                    break status;
                }
            };
            // Read what is left of its output for a moment: a grandchild still holding the pipe never keeps it.
            let deadline = Instant::now() + Duration::from_secs(1);
            while let Ok(line) = lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                if let Some((repo, file, done, total)) = parse_progress(&line) {
                    files.insert(format!("{repo}/{file}"), (done, total));
                    fetched.insert((repo.clone(), file));
                    repos.insert(repo);
                    pending = true;
                } else if !line.trim().is_empty() {
                    last_words = line;
                }
            }
            if pending {
                let _ = events.send(progress(&files));
            }
            // Joined only when they are done; one blocked on an inherited pipe is left to end with it.
            for t in [reader, drain] {
                if t.is_finished() {
                    let _ = t.join();
                }
            }
            let canceled = stop.load(Ordering::Acquire);
            if status.success() && !canceled {
                match record_download(&roots_, &source_, &fetched) {
                    Ok(n) => eprintln!("Engine models: {source_}: {n} downloaded file(s) recorded"),
                    Err(e) => eprintln!("Engine models: {source_}: could not record its downloaded files: {e:#}"),
                }
            }
            if canceled && discard_partials.load(Ordering::Acquire) {
                let freed = remove_partials(&roots_.hf, &repos);
                eprintln!("Engine models: canceled the download of {id_} {quant_}; removed {freed} bytes of partial files");
            }
            let result = if status.success() && !canceled {
                Ok(())
            } else if canceled {
                Err("Canceled".to_string())
            } else {
                // Splash's lines start with a HH:MM:SS time; anyhow's with "Error: ".
                let words = last_words.trim();
                let words = words.split_once(' ').filter(|(t, _)| t.len() == 8 && t.as_bytes()[2] == b':').map_or(words, |(_, r)| r);
                let words = words.strip_prefix("Error: ").unwrap_or(words);
                Err(if words.is_empty() { format!("The download stopped ({status})") } else { words.chars().take(300).collect() })
            };
            let _ = events.send(DownloadEvent::Done { id: id_, quant: quant_, result, canceled });
        });
        Ok(Self { id: id.into(), quant: quant.into(), source: source.into(), cancel, discard, thread: Some(thread) })
    }
    /// Stop it: SIGTERM to its process group, SIGKILL after three seconds, then its partial files go. Its `Done`
    /// follows.
    pub fn cancel(&self) {
        self.discard.store(true, Ordering::Release);
        self.cancel.store(true, Ordering::Release);
    }
}
impl Drop for Download {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The partial downloads (`blobs/<etag>.incomplete`, hub/cache.rs `fetch`) a canceled installer left in the
/// repositories it was downloading. One another download still holds (its `.locks` entry) stays. Bytes freed.
pub fn remove_partials(hf: &Path, repos: &BTreeSet<String>) -> u64 {
    let Ok(hf) = hf.canonicalize() else { return 0 };
    let mut freed = 0;
    for repo in repos {
        if check_source(repo).is_err() {
            continue;
        }
        let folder = format!("models--{}", repo.replace('/', "--"));
        let Ok(blobs) = hf.join(&folder).join("blobs").canonicalize() else { continue };
        if blobs != hf.join(&folder).join("blobs") {
            continue;
        }
        for entry in fs::read_dir(&blobs).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(etag) = name.strip_suffix(".incomplete") else { continue };
            let Ok(meta) = fs::symlink_metadata(entry.path()) else { continue };
            if !meta.is_file() {
                continue;
            }
            let lock = hf.join(".locks").join(&folder).join(format!("{etag}.lock"));
            if let Ok(f) = File::open(&lock) {
                if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    continue;
                }
            }
            if fs::remove_file(entry.path()).is_ok() {
                freed += meta.len();
            }
        }
    }
    freed
}

// ---- delete

fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}
/// The refs/splash folder of a selection link's pins (hub/cache.rs `pin_owner`): the SHA-256 of its path with a
/// resolved parent.
pub fn pin_owner(link: &Path) -> String {
    let parent = link.parent().unwrap_or(Path::new("/"));
    let parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
    sha256_hex(parent.join(link.file_name().unwrap_or_default()).as_os_str().as_bytes())
}
fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every selection link under a models root: `*/*` symlinks (`.selections` included), as assembly.rs
/// `selection_links` finds them.
fn selection_links(root: &Path) -> Vec<PathBuf> {
    let mut links = Vec::new();
    for dir in fs::read_dir(root).into_iter().flatten().flatten() {
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

/// Try an exclusive flock for about a second. A lock can stay shared for a moment after its holder lets go, while a
/// process this one forks has not exec'd yet (its copy of the descriptor closes at exec).
fn try_exclusive(f: &File) -> bool {
    for attempt in 0..10 {
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return true;
        }
        if attempt < 9 {
            thread::sleep(Duration::from_millis(100));
        }
    }
    false
}
/// A server holds the assembly it serves with a shared flock on its model.json (assembly.rs `hold`).
fn held(assembly: &Path) -> bool {
    let Ok(record) = File::open(assembly.join("model.json")) else { return false };
    !try_exclusive(&record)
}

/// Splash's installation lock (`<models root>/.install.lock`, assembly.rs `installation_lock`), without waiting long:
/// an installation in progress refuses the delete.
struct InstallLock(File);
impl InstallLock {
    fn acquire(models: &Path) -> Result<Self> {
        let f = OpenOptions::new().create(true).append(true).read(true).open(models.join(".install.lock"))?;
        for _ in 0..20 {
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(f));
            }
            thread::sleep(Duration::from_millis(100));
        }
        bail!("a model installation is running")
    }
}
impl Drop for InstallLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A file an assembly links from the hub cache, resolved inside the cache root.
#[derive(Debug)]
struct CacheFile {
    /// The snapshot entry with resolved folders (`<hf>/models--o--r/snapshots/<commit>/<name>`).
    pointer: PathBuf,
    repo: PathBuf,
    commit: String,
    snapshot: PathBuf,
    /// The blob it links (`<hf>/models--o--r/blobs/<etag>`); `None` when the entry is a plain file or dangles.
    blob: Option<PathBuf>,
}
/// `recorded` (an absolute path from model.json) as a file of the hub cache `hf` (canonical). Anything outside it,
/// or reached through a link out of it, is refused.
fn cache_file(hf: &Path, recorded: &Path) -> Result<CacheFile> {
    let snapshot = recorded
        .ancestors()
        .skip(1)
        .find(|p| p.parent().and_then(Path::file_name).is_some_and(|n| n == "snapshots"))
        .context("not in a hub cache snapshot")?;
    let commit = snapshot.file_name().unwrap_or_default().to_string_lossy().into_owned();
    ensure!(is_hex(&commit, 40), "not a snapshot commit");
    let repo = snapshot.parent().and_then(Path::parent).context("no repository folder")?;
    let folder = repo.file_name().unwrap_or_default().to_string_lossy().into_owned();
    ensure!(folder.starts_with("models--"), "not a model repository folder");
    let repo = repo.canonicalize()?;
    ensure!(repo == hf.join(&folder), "outside the hub cache");
    let real_snapshot = snapshot.canonicalize()?;
    ensure!(real_snapshot == repo.join("snapshots").join(&commit), "snapshot reached through a link");
    let parent = recorded.parent().context("no folder")?.canonicalize()?;
    ensure!(parent.starts_with(&real_snapshot), "snapshot file reached through a link");
    let pointer = parent.join(recorded.file_name().context("no name")?);
    let meta = fs::symlink_metadata(&pointer)?;
    ensure!(!meta.is_dir(), "a folder, not a file");
    let blob = if meta.file_type().is_symlink() {
        match pointer.canonicalize() {
            Ok(blob) => {
                ensure!(blob.parent() == Some(repo.join("blobs").as_path()), "links outside its repository's blobs");
                ensure!(fs::symlink_metadata(&blob)?.is_file(), "its blob is not a file");
                Some(blob)
            }
            Err(_) => None,
        }
    } else {
        None
    };
    Ok(CacheFile { pointer, repo, commit, snapshot: real_snapshot, blob })
}

fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return if meta.is_file() { meta.len() } else { 0 };
    }
    fs::read_dir(path).into_iter().flatten().flatten().map(|e| tree_bytes(&e.path())).sum()
}
/// Every symlink under a folder, recursively.
fn links_under(path: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).into_iter().flatten().flatten() {
        let Ok(t) = entry.file_type() else { continue };
        if t.is_symlink() {
            out.push(entry.path());
        } else if t.is_dir() {
            links_under(&entry.path(), out);
        }
    }
}

/// What a delete removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Deleted {
    pub bytes: u64,
    pub paths: Vec<PathBuf>,
}

// ---- manifests: what Heimdall's own pulls downloaded

/// One hub-cache file a Heimdall pull downloaded for an installation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ManifestFile {
    pub repo: String,
    pub file: String,
    /// Its snapshot entry and the blob it linked, as resolved when it was recorded.
    pub pointer: PathBuf,
    pub blob: Option<PathBuf>,
    /// The file it was when recorded; a delete removes it only while it still is (`None`: never).
    #[serde(default)]
    pub identity: Option<FileIdentity>,
}
/// Device, inode, modification time and size of the file a cache entry resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
    pub mtime_ns: i64,
    pub size: u64,
}
impl FileIdentity {
    /// What `pointer` resolves to now.
    pub fn of(pointer: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let m = fs::metadata(pointer).ok()?;
        Some(Self { dev: m.dev() as u64, ino: m.ino(), mtime_ns: m.mtime() * 1_000_000_000 + m.mtime_nsec(), size: m.len() })
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// The installation's selection link, relative to the models root (`OWNER/REPO:VARIANT`).
    pub source: String,
    pub files: BTreeSet<ManifestFile>,
}
/// `<manifests>/<sha256 of the link relative to the models root>.json`.
pub fn manifest_path(roots: &Roots, relative: &str) -> PathBuf {
    roots.manifests.join(format!("{}.json", sha256_hex(relative.as_bytes())))
}
pub fn read_manifest(roots: &Roots, relative: &str) -> Manifest {
    fs::read(manifest_path(roots, relative))
        .ok()
        .and_then(|b| serde_json::from_slice::<Manifest>(&b).ok())
        .filter(|m| m.source == relative)
        .unwrap_or_else(|| Manifest { source: relative.into(), files: BTreeSet::new() })
}
/// Written whole (0600, under a unique staging name, then renamed into place); an empty one is removed.
pub(crate) fn write_manifest(roots: &Roots, m: &Manifest) -> Result<()> {
    let path = manifest_path(roots, &m.source);
    if m.files.is_empty() {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => return Ok(()),
        }
    }
    crate::settings::private_dir(&roots.manifests)?;
    let staging = path.with_extension(format!("json.{}.{}", std::process::id(), uuid::Uuid::new_v4().simple()));
    crate::settings::private_write(&staging, &serde_json::to_vec_pretty(m)?)?;
    fs::rename(&staging, &path)?;
    Ok(())
}
/// The repository of a hub-cache folder (`models--o--r` → `o/r`; ids never hold `--`, `check_source`).
fn folder_repo(folder: &str) -> Option<String> {
    folder.strip_prefix("models--")?.split_once("--").map(|(o, r)| format!("{o}/{r}"))
}
/// After a successful pull of `source`: the files of its assembly that pull fetched (`fetched`, from its progress
/// lines) join its manifest. Returns how many it lists.
pub fn record_download(roots: &Roots, source: &str, fetched: &BTreeSet<(String, String)>) -> Result<usize> {
    check_source(source)?;
    let assembly = roots.models.join(source).canonicalize().context("no installation after the pull")?;
    let record = read_record(&assembly).context("no assembly record after the pull")?;
    let hf = roots.hf.canonicalize()?;
    let mut manifest = read_manifest(roots, source);
    manifest.files.retain(|f| fs::symlink_metadata(&f.pointer).is_ok());
    for entry in record["files"].as_object().into_iter().flat_map(|f| f.values()) {
        let Some(path) = entry["path"].as_str().map(Path::new) else { continue };
        let Ok(f) = cache_file(&hf, path) else { continue };
        let repo = folder_repo(&f.repo.file_name().unwrap_or_default().to_string_lossy());
        let file = f.pointer.strip_prefix(&f.snapshot).ok().map(|p| p.to_string_lossy().into_owned());
        if let (Some(repo), Some(file)) = (repo, file) {
            if fetched.contains(&(repo.clone(), file.clone())) {
                let identity = FileIdentity::of(&f.pointer);
                manifest.files.insert(ManifestFile { repo, file, pointer: f.pointer, blob: f.blob, identity });
            }
        }
    }
    write_manifest(roots, &manifest)?;
    Ok(manifest.files.len())
}

/// Derived-metadata keys are SHA-256 hex (assembly.rs `key_of`); anything else is not one.
fn metadata_key(path: &Path, metadata: &Path) -> Option<std::ffi::OsString> {
    let first = path.strip_prefix(metadata).ok()?.components().next()?.as_os_str().to_os_string();
    let s = first.to_str()?;
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(first)
}

/// Another Splash's installation lock (Homebrew Splash's root), tried without creating it.
fn try_other_lock(root: &Path) -> Result<Option<File>> {
    let Ok(f) = File::open(root.join(".install.lock")) else { return Ok(None) };
    ensure!(try_exclusive(&f), "another Splash is installing a model ({})", root.display());
    Ok(Some(f))
}

/// Delete the precision `source`: its selection link and refs/splash pins; its assembly and derived metadata when
/// no other link names the assembly; and of the hub-cache files only those Heimdall's own pull downloaded for it
/// (its manifest, `record_download`) that nothing else uses. A cache file stays when another assembly (under
/// `roots.models` or any of `roots.others`) links it or its blob, when another snapshot entry links its blob, when its
/// snapshot is pinned by an installation these roots do not show, or when it is not in this precision's manifest
/// (another tool may use it). A kept file another Heimdall installation uses moves to that installation's manifest.
/// Refused while a server holds the assembly or an installation runs (here or in another root). Only exact paths
/// inside the roots are removed.
pub fn delete(roots: &Roots, source: &str) -> Result<Deleted> {
    check_source(source)?;
    let models = roots.models.canonicalize().context("not on this Mac")?;
    let hf = roots.hf.canonicalize().ok();
    let _lock = InstallLock::acquire(&models)?;
    let _other_locks = roots.others.iter().map(|r| try_other_lock(r)).collect::<Result<Vec<_>>>()?;
    let (owner, name) = source.split_once('/').context("source")?;
    let link_dir = models.join(owner).canonicalize().context("not on this Mac")?;
    ensure!(link_dir == models.join(owner), "its folder is reached through a link");
    let link = link_dir.join(name);
    let meta = fs::symlink_metadata(&link).context("not on this Mac")?;
    ensure!(meta.file_type().is_symlink(), "{} is not a Splash selection link", link.display());
    let resolved = models.join(".resolved");
    let assembly = link.canonicalize().ok().filter(|a| {
        a.parent() == Some(resolved.as_path()) && a.file_name().is_some_and(|n| is_hex(&n.to_string_lossy(), 64))
    });
    if let Some(a) = &assembly {
        ensure!(!held(a), "the engine is using it");
    }
    let record = assembly.as_deref().and_then(read_record);
    let ours = pin_owner(&link);
    let metadata = models.join(".metadata");
    let manifest = read_manifest(roots, source);

    // What this precision uses, every path checked before anything goes.
    let mut keys = BTreeSet::new();
    let mut cache_files = Vec::new();
    for entry in record.as_ref().and_then(|r| r["files"].as_object()).into_iter().flat_map(|f| f.values()) {
        let Some(path) = entry["path"].as_str().map(Path::new) else { continue };
        if path.starts_with(&metadata) {
            match metadata_key(path, &metadata) {
                Some(key) => drop(keys.insert(key)),
                None => eprintln!("Engine models: kept {} (not a metadata entry)", path.display()),
            }
            continue;
        }
        let Some(hf) = &hf else {
            eprintln!("Engine models: kept {} (no hub cache at {})", path.display(), roots.hf.display());
            continue;
        };
        match cache_file(hf, path) {
            Ok(f) => {
                if !cache_files.iter().any(|c: &CacheFile| c.pointer == f.pointer) {
                    cache_files.push(f);
                }
            }
            Err(e) => eprintln!("Engine models: kept {}: {e:#}", path.display()),
        }
    }
    let snapshots: BTreeSet<(PathBuf, String)> = cache_files.iter().map(|f| (f.repo.clone(), f.commit.clone())).collect();
    let mut deleted = Deleted::default();
    let remove_pins = |deleted: &mut Deleted| -> Result<()> {
        for (repo, commit) in &snapshots {
            let owner_dir = repo.join("refs/splash").join(&ours);
            let pin = owner_dir.join(commit);
            if fs::symlink_metadata(&pin).is_ok_and(|m| m.is_file()) {
                fs::remove_file(&pin)?;
                deleted.paths.push(pin);
            }
            let _ = fs::remove_dir(&owner_dir);
        }
        Ok(())
    };

    // What everything else uses; for links under Heimdall's root, which link uses each cache entry.
    let mut known_owners = BTreeSet::new();
    let mut sharing = Vec::new();
    let mut users: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    let mut other_assemblies = BTreeSet::new();
    for root in std::iter::once(&models).chain(&roots.others) {
        for l in selection_links(root) {
            if l == link {
                continue;
            }
            known_owners.insert(pin_owner(&l));
            let Ok(target) = l.canonicalize() else { continue };
            let relative = (root == &models).then(|| l.strip_prefix(&models).ok().map(|r| r.to_string_lossy().into_owned())).flatten();
            if Some(&target) == assembly.as_ref() {
                sharing.extend(relative.clone());
            }
            if let (Some(rel), Some(r)) = (&relative, read_record(&target)) {
                for entry in r["files"].as_object().into_iter().flat_map(|f| f.values()) {
                    let Some(path) = entry["path"].as_str().map(Path::new) else { continue };
                    if let Some(parent) = path.parent().and_then(|p| p.canonicalize().ok()) {
                        users.entry(parent.join(path.file_name().unwrap_or_default())).or_default().insert(rel.clone());
                    }
                }
            }
            other_assemblies.insert(target);
        }
    }
    let move_to = |file: &ManifestFile, links: &BTreeSet<String>| {
        for rel in links {
            let mut m = read_manifest(roots, rel);
            if m.files.insert(file.clone()) {
                if let Err(e) = write_manifest(roots, &m) {
                    eprintln!("Engine models: could not hand {} to {rel}: {e:#}", file.pointer.display());
                }
            }
        }
    };

    // Another link names the same assembly: only this link and its pins go; its files stay in use.
    if assembly.is_some() && other_assemblies.contains(assembly.as_ref().unwrap()) {
        fs::remove_file(&link)?;
        deleted.paths.push(link.clone());
        remove_pins(&mut deleted)?;
        let all: BTreeSet<String> = sharing.into_iter().collect();
        for f in &manifest.files {
            move_to(f, &all);
        }
        write_manifest(roots, &Manifest { source: source.into(), files: BTreeSet::new() })?;
        return Ok(deleted);
    }
    // Assemblies no link names may still be held by a server: in every root, they count as users too.
    let other_resolved = roots.others.iter().filter_map(|r| r.canonicalize().ok()).map(|r| r.join(".resolved"));
    for dir in std::iter::once(resolved.clone()).chain(other_resolved) {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if Some(&p) != assembly.as_ref() && p.is_dir() {
                other_assemblies.insert(p);
            }
        }
    }
    let (mut used_pointers, mut used_blobs, mut used_keys) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for a in &other_assemblies {
        let Some(r) = read_record(a) else { continue };
        for entry in r["files"].as_object().into_iter().flat_map(|f| f.values()) {
            let Some(path) = entry["path"].as_str().map(Path::new) else { continue };
            if path.starts_with(&metadata) {
                used_keys.extend(metadata_key(path, &metadata));
                continue;
            }
            if let Some(parent) = path.parent().and_then(|p| p.canonicalize().ok()) {
                used_pointers.insert(parent.join(path.file_name().unwrap_or_default()));
            }
            if let Ok(blob) = path.canonicalize() {
                used_blobs.insert(blob);
            }
        }
    }
    // Other installations' manifests, while their links stand, keep their files too.
    for entry in fs::read_dir(&roots.manifests).into_iter().flatten().flatten() {
        let Some(m) = fs::read(entry.path()).ok().and_then(|b| serde_json::from_slice::<Manifest>(&b).ok()) else { continue };
        if m.source != source && fs::symlink_metadata(models.join(&m.source)).is_ok() {
            used_pointers.extend(m.files.iter().map(|f| f.pointer.clone()));
            used_blobs.extend(m.files.iter().filter_map(|f| f.blob.clone()));
        }
    }
    // Snapshots an installation these roots do not show pins.
    let mut protected = BTreeSet::new();
    for (repo, commit) in &snapshots {
        for owner_dir in fs::read_dir(repo.join("refs/splash")).into_iter().flatten().flatten() {
            let owner_name = owner_dir.file_name().to_string_lossy().into_owned();
            if owner_name != ours && !known_owners.contains(&owner_name) && owner_dir.path().join(commit).exists() {
                protected.insert((repo.clone(), commit.clone()));
            }
        }
    }

    fs::remove_file(&link)?;
    deleted.paths.push(link.clone());
    if let Some(a) = &assembly {
        deleted.bytes += tree_bytes(a);
        fs::remove_dir_all(a)?;
        deleted.paths.push(a.clone());
        for key in keys.iter().filter(|k| !used_keys.contains(*k)) {
            let entry = metadata.join(key);
            if fs::symlink_metadata(&entry).is_ok_and(|m| m.is_dir()) && entry.parent() == Some(metadata.as_path()) {
                deleted.bytes += tree_bytes(&entry);
                fs::remove_dir_all(&entry)?;
                deleted.paths.push(entry);
            }
        }
    }
    for f in &cache_files {
        let Some(owned) = manifest.files.iter().find(|m| m.pointer == f.pointer) else { continue };
        if protected.contains(&(f.repo.clone(), f.commit.clone())) || used_pointers.contains(&f.pointer) {
            move_to(owned, users.get(&f.pointer).unwrap_or(&BTreeSet::new()));
            continue;
        }
        // Not the file the pull recorded any more (replaced, rewritten): kept, and its entry dropped.
        if owned.identity.is_none() || FileIdentity::of(&f.pointer) != owned.identity {
            eprintln!("Engine models: kept {} (changed since Heimdall downloaded it)", f.pointer.display());
            continue;
        }
        // The hub's download lock for this blob, as Splash's fetch takes it (hub/cache.rs `download_one`): a pull
        // in flight elsewhere keeps the file; the links are checked again under it.
        let etag = f.blob.as_ref().and_then(|b| b.file_name()).map(|n| n.to_string_lossy().into_owned());
        let folder = f.repo.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let _download_lock = match etag.as_ref().filter(|e| is_hex(e, 40) || is_hex(e, 64)) {
            Some(etag) => {
                let lock_path = hf.as_ref().expect("cache files come from the hub cache").join(".locks").join(&folder).join(format!("{etag}.lock"));
                let lock = fs::create_dir_all(lock_path.parent().expect("a folder"))
                    .and_then(|()| OpenOptions::new().create(true).append(true).read(true).open(&lock_path));
                match lock {
                    Ok(l) if try_exclusive(&l) => Some(l),
                    _ => {
                        eprintln!("Engine models: kept {} (a download holds it)", f.pointer.display());
                        continue;
                    }
                }
            }
            None => None,
        };
        let plain = fs::symlink_metadata(&f.pointer).is_ok_and(|m| m.is_file());
        if plain {
            deleted.bytes += fs::metadata(&f.pointer).map(|m| m.len()).unwrap_or(0);
        }
        fs::remove_file(&f.pointer)?;
        deleted.paths.push(f.pointer.clone());
        // The blob only when it is the one the pull recorded and no snapshot entry links it, checked under the lock.
        if let Some(blob) = f.blob.as_ref().filter(|b| !used_blobs.contains(*b) && owned.blob.as_ref() == Some(*b)) {
            let mut pointers = Vec::new();
            links_under(&f.repo.join("snapshots"), &mut pointers);
            if !pointers.iter().any(|p| p.canonicalize().ok().as_ref() == Some(blob)) {
                deleted.bytes += fs::metadata(blob).map(|m| m.len()).unwrap_or(0);
                fs::remove_file(blob)?;
                deleted.paths.push(blob.clone());
            }
        }
        // Folders the entry leaves empty, up to its snapshot.
        let mut dir = f.pointer.parent().map(Path::to_path_buf);
        while let Some(d) = dir.filter(|d| d.starts_with(&f.snapshot)) {
            if fs::remove_dir(&d).is_err() {
                break;
            }
            dir = d.parent().map(Path::to_path_buf);
        }
    }
    remove_pins(&mut deleted)?;
    write_manifest(roots, &Manifest { source: source.into(), files: BTreeSet::new() })?;
    Ok(deleted)
}

/// Test fixtures: a Splash installation laid out as `heimdall-splash pull` leaves it, with tiny files.
#[cfg(test)]
pub(crate) mod fixture {
    use super::*;
    use std::os::unix::fs::{symlink, MetadataExt};

    /// A file of an installation: the repository, the file's name in it, its assembly path and its bytes.
    pub struct File<'a> {
        pub repo: &'a str,
        pub name: &'a str,
        pub at: &'a str,
        pub bytes: &'a [u8],
    }
    pub fn commit(repo: &str) -> String {
        sha256_hex(repo.as_bytes())[..40].to_string()
    }
    /// The cache entry of a file (`snapshots/<commit>/<name>` → `../../blobs/<sha256>`), shared when it exists; and
    /// whether this call made it (what a pull would have downloaded).
    pub fn cache_entry(hf: &Path, repo: &str, name: &str, bytes: &[u8]) -> (PathBuf, bool) {
        let folder = hf.join(format!("models--{}", repo.replace('/', "--")));
        let blob = folder.join("blobs").join(sha256_hex(bytes));
        fs::create_dir_all(blob.parent().unwrap()).unwrap();
        // A blob is written once, as the hub does; a later install reuses it untouched.
        if !blob.exists() {
            fs::write(&blob, bytes).unwrap();
        }
        let pointer = folder.join("snapshots").join(commit(repo)).join(name);
        fs::create_dir_all(pointer.parent().unwrap()).unwrap();
        let made = fs::symlink_metadata(&pointer).is_err();
        if made {
            let depth = name.split('/').count();
            symlink(format!("{}blobs/{}", "../".repeat(depth + 1), sha256_hex(bytes)), &pointer).unwrap();
        }
        (pointer, made)
    }
    /// Install `source` with these files as a pull does: cache entries, the assembly (with a derived metadata entry),
    /// the pins and the selection link; with `recorded`, the Heimdall manifest of the files it made (false: another
    /// tool's installation).
    pub fn install_as(roots: &Roots, source: &str, files: &[File], recorded: bool) -> PathBuf {
        let models = {
            fs::create_dir_all(&roots.models).unwrap();
            roots.models.canonicalize().unwrap()
        };
        fs::create_dir_all(&roots.hf).unwrap();
        let hf = roots.hf.canonicalize().unwrap();
        let key = sha256_hex(format!("meta {source}").as_bytes());
        let meta = models.join(".metadata").join(&key);
        fs::create_dir_all(&meta).unwrap();
        fs::write(meta.join("config.json"), b"{}").unwrap();
        let mut fetched = BTreeSet::new();
        let mut linked: Vec<(String, PathBuf)> = files.iter().map(|f| {
            let (pointer, made) = cache_entry(&hf, f.repo, f.name, f.bytes);
            if made { fetched.insert((f.repo.to_string(), f.name.to_string())); }
            (f.at.to_string(), pointer)
        }).collect();
        linked.push(("tokenizer/config.json".into(), meta.join("config.json")));
        let mut record_files = serde_json::Map::new();
        for (at, path) in &linked {
            let stat = fs::metadata(path).unwrap();
            record_files.insert(at.clone(), serde_json::json!({"path": path.display().to_string(), "bytes": stat.len(),
                "mtime_ns": stat.mtime() * 1_000_000_000 + stat.mtime_nsec(), "ctime_ns": stat.ctime() * 1_000_000_000 + stat.ctime_nsec(),
                "digest": sha256_hex(&fs::read(path).unwrap())}));
        }
        let target = source.split(':').next().unwrap();
        let draft = files.iter().map(|f| f.repo).find(|r| *r != target).unwrap_or(target);
        let record = serde_json::json!({"version": 1, "model": source, "family": "Qwen3.8-27B", "target_format": "gguf",
            "vision_format": "gguf", "metadata": key, "files": record_files,
            "sources": {"target": {"repo": target, "revision": commit(target)}, "draft": {"repo": draft, "revision": commit(draft)}}});
        let encoded = serde_json::to_vec(&record).unwrap();
        let assembly = models.join(".resolved").join(sha256_hex(&encoded));
        fs::create_dir_all(&assembly).unwrap();
        for (at, path) in &linked {
            let l = assembly.join(at);
            fs::create_dir_all(l.parent().unwrap()).unwrap();
            symlink(path, &l).unwrap();
        }
        fs::write(assembly.join("model.json"), &encoded).unwrap();
        let link = models.join(source);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&assembly, &link).unwrap();
        for repo in files.iter().map(|f| f.repo).collect::<BTreeSet<_>>() {
            let pin = hf.join(format!("models--{}", repo.replace('/', "--"))).join("refs/splash").join(pin_owner(&link)).join(commit(repo));
            fs::create_dir_all(pin.parent().unwrap()).unwrap();
            fs::write(pin, commit(repo)).unwrap();
        }
        if recorded {
            record_download(roots, source, &fetched).unwrap();
        }
        assembly
    }
    /// A typical precision: its GGUF, the model's projector (same repository) and a draft repository's two files.
    pub fn install_quant(roots: &Roots, source: &str, draft: &str) -> PathBuf {
        install_quant_as(roots, source, draft, true)
    }
    pub fn install_quant_as(roots: &Roots, source: &str, draft: &str, record: bool) -> PathBuf {
        let (repo, quant) = source.split_once(':').unwrap();
        let gguf = format!("{}-{quant}.gguf", repo.rsplit('/').next().unwrap());
        let weights = format!("weights of {source}");
        install_as(roots, source, &[
            File { repo, name: &gguf, at: &format!("target/{gguf}"), bytes: weights.as_bytes() },
            File { repo, name: "mmproj.gguf", at: "vision/mmproj.gguf", bytes: format!("projector of {repo}").as_bytes() },
            File { repo: draft, name: "config.json", at: "draft/config.json", bytes: format!("draft config {draft}").as_bytes() },
            File { repo: draft, name: "model.safetensors", at: "draft/model.safetensors", bytes: format!("draft weights {draft}").as_bytes() },
        ], record)
    }
}
