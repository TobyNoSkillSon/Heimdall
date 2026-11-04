//! The Hugging Face Hub and its cache, without Python (install/hub.py and the parts of huggingface_hub it uses).
//!
//! Files land in the standard cache layout (`models--o--r/{blobs,snapshots/<commit>,refs}`), with huggingface_hub's
//! lock files and tree listings, so a file Splash, huggingface_hub or anything else already downloaded is reused and
//! never fetched again, and what this downloads they reuse too. A blob is named by the digest the Hub publishes for
//! it (the LFS SHA-256, or the git blob SHA-1 of a small file), and every download is checked against that digest
//! before it is published.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use super::{is_hex, is_safe_path, json as pyjson, Error, Event, Result};

/// Seconds the Hub may take to resolve a revision before the installed assembly starts without it.
const HUB_TIMEOUT: Duration = Duration::from_secs(5);
/// Bytes per range request when reading a header that is not cached (HfFileSystem's block size in hub.py).
const RANGE_BLOCK: u64 = 8 << 20;

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// The SHA-256 of a file's content.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

/// The git blob SHA-1 of a file, the ID the Hub gives a small (non-LFS) file.
pub fn git_sha1_file(path: &Path) -> Result<String> {
    let size = fs::metadata(path)?.len();
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {size}\0").as_bytes());
    io::copy(&mut File::open(path)?, &mut hasher)?;
    Ok(hex::encode(hasher.finalize()))
}

fn truthy(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| ["1", "ON", "YES", "TRUE"].contains(&v.to_uppercase().as_str()))
}

fn expand_home(value: String) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None if value == "~" => home(),
        None => PathBuf::from(value),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// HF_HOME, else $XDG_CACHE_HOME/huggingface, else ~/.cache/huggingface (huggingface_hub.constants).
pub fn hf_home() -> PathBuf {
    if let Ok(home) = std::env::var("HF_HOME") {
        return expand_home(home);
    }
    let cache = std::env::var("XDG_CACHE_HOME").map(expand_home).unwrap_or_else(|_| home().join(".cache"));
    cache.join("huggingface")
}

/// HF_HUB_CACHE, else HUGGINGFACE_HUB_CACHE, else HF_HOME/hub.
pub fn default_cache() -> PathBuf {
    std::env::var("HF_HUB_CACHE")
        .or_else(|_| std::env::var("HUGGINGFACE_HUB_CACHE"))
        .map(expand_home)
        .unwrap_or_else(|_| hf_home().join("hub"))
}

/// The token Hub requests carry: HF_TOKEN (or HUGGING_FACE_HUB_TOKEN), else the one `hf auth login` saved.
fn token() -> Option<String> {
    let clean = |t: String| Some(t.replace(['\r', '\n'], "").trim().to_string()).filter(|t| !t.is_empty());
    let env = std::env::var("HF_TOKEN").or_else(|_| std::env::var("HUGGING_FACE_HUB_TOKEN")).ok().and_then(clean);
    env.or_else(|| {
        let path = std::env::var("HF_TOKEN_PATH").map(expand_home).unwrap_or_else(|_| hf_home().join("token"));
        fs::read_to_string(path).ok().and_then(clean)
    })
}

/// The folder of a model repository in any Hub cache.
pub fn folder_name(repo_id: &str) -> String {
    format!("models--{}", repo_id.replace('/', "--"))
}

/// The Hub snapshot folder a cached file's path lies in; None outside any Hub cache.
pub fn snapshot_of(path: &Path) -> Option<PathBuf> {
    let path = std::path::absolute(path).ok()?;
    path.ancestors().skip(1).find(|p| p.parent().and_then(Path::file_name).is_some_and(|n| n == "snapshots")).map(
        Path::to_path_buf,
    )
}

/// Percent-encode a URL path, keeping `/` unless `slash` is false.
fn quote(value: &str, slash: bool) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) || (slash && b == b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The Hub, its cache and how to reach it.
pub struct Hub {
    pub cache: PathBuf,
    pub endpoint: String,
    /// HF_HUB_OFFLINE: no request is ever made.
    pub offline: bool,
    token: Option<String>,
    client: OnceLock<std::result::Result<reqwest::blocking::Client, String>>,
}

impl Hub {
    pub fn new(cache: Option<PathBuf>, offline: bool) -> Hub {
        Hub {
            cache: cache.unwrap_or_else(default_cache),
            endpoint: std::env::var("HF_ENDPOINT")
                .map(|e| e.trim_end_matches('/').to_string())
                .unwrap_or_else(|_| "https://huggingface.co".into()),
            offline: offline || truthy("HF_HUB_OFFLINE") || truthy("TRANSFORMERS_OFFLINE"),
            token: token(),
            client: OnceLock::new(),
        }
    }

    pub fn folder(&self, repo_id: &str) -> PathBuf {
        self.cache.join(folder_name(repo_id))
    }

    pub fn snapshot(&self, repo_id: &str, commit: &str) -> PathBuf {
        self.folder(repo_id).join("snapshots").join(commit)
    }

    fn client(&self) -> Result<&reqwest::blocking::Client> {
        self.client
            .get_or_init(|| {
                reqwest::blocking::Client::builder()
                    .user_agent(concat!("heimdall-splash/", env!("CARGO_PKG_VERSION")))
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(None)
                    .build()
                    .map_err(|e| e.to_string())
            })
            .as_ref()
            .map_err(|e| Error::model(format!("cannot start an HTTP client: {e}")))
    }

    /// A request with the token, which reqwest drops when a redirect leaves the Hub's host.
    fn get(&self, url: &str) -> Result<reqwest::blocking::RequestBuilder> {
        if self.offline {
            return Err(Error::Hub("HF_HUB_OFFLINE is set".into()));
        }
        let request = self.client()?.get(url);
        Ok(match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        })
    }

    /// The Hub's reason for a failure on one line, with the token redacted and, for denied access, how to fix it.
    fn reason(&self, error: impl std::fmt::Display, status: Option<u16>) -> String {
        let mut message = error.to_string().split_whitespace().collect::<Vec<_>>().join(" ");
        if let Some(token) = &self.token {
            message = message.replace(token, "[redacted]");
        }
        if matches!(status, Some(401 | 403)) {
            message += "; set HF_TOKEN or run 'hf auth login' with access to this repository";
        }
        message
    }

    fn send(&self, request: reqwest::blocking::RequestBuilder) -> Result<reqwest::blocking::Response> {
        let response = request.send().map_err(|e| Error::Hub(self.reason(e, None)))?;
        let status = response.status();
        if status.is_success() || status.as_u16() == 416 {
            return Ok(response);
        }
        let url = response.url().clone();
        let body = response.text().unwrap_or_default();
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        Err(Error::Hub(self.reason(format!("{status} for {url} {detail}"), Some(status.as_u16()))))
    }

    /// The commit a revision names now and the repository's files at it, with sizes and blob IDs
    /// (`HfApi.model_info(files_metadata=True)`: GET /api/models/{repo}[/revision/{rev}]?blobs=true).
    pub fn model_info(&self, repo_id: &str, revision: Option<&str>) -> Result<Listing> {
        let mut url = format!("{}/api/models/{repo_id}", self.endpoint);
        if let Some(revision) = revision {
            url += &format!("/revision/{}", quote(revision, false));
        }
        let response = self.send(self.get(&url)?.query(&[("blobs", "true")]).timeout(HUB_TIMEOUT))?;
        let info: Value = response.json().map_err(|e| Error::Hub(self.reason(e, None)))?;
        let commit = info["sha"].as_str().unwrap_or_default().to_string();
        if !is_hex(&commit, 40) {
            return Err(Error::model(format!("the Hub did not resolve {repo_id} to a commit")));
        }
        let mut files = BTreeMap::new();
        for sibling in info["siblings"].as_array().into_iter().flatten() {
            let Some(name) = sibling["rfilename"].as_str() else { continue };
            let lfs = sibling["lfs"]["sha256"].as_str();
            files.insert(
                name.to_string(),
                Blob {
                    size: sibling["size"].as_u64(),
                    blob_id: sibling["blobId"].as_str().map(str::to_string),
                    lfs_sha256: lfs.map(str::to_string),
                    lfs_size: sibling["lfs"]["size"].as_u64(),
                },
            );
        }
        Ok(Listing { commit, files })
    }

    /// The tree listing huggingface_hub caches for a commit (`trees/<commit>.json`), if any.
    pub fn read_tree(&self, repo_id: &str, commit: &str) -> Option<BTreeMap<String, Blob>> {
        let path = self.folder(repo_id).join("trees").join(format!("{commit}.json"));
        let data: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
        if data["format_version"] != json!(1) {
            return None;
        }
        let files = data["files"].as_object()?;
        Some(
            files
                .iter()
                .map(|(name, info)| {
                    let blob = Blob {
                        size: info["size"].as_u64(),
                        blob_id: info["blob_id"].as_str().map(str::to_string),
                        lfs_sha256: info["lfs_sha256"].as_str().map(str::to_string),
                        lfs_size: info["lfs_size"].as_u64(),
                    };
                    (name.clone(), blob)
                })
                .collect(),
        )
    }

    /// Cache a commit's listing as huggingface_hub does, so its offline reads and ours see the whole tree.
    fn write_tree(&self, repo_id: &str, listing: &Listing) {
        let folder = self.folder(repo_id).join("trees");
        let path = folder.join(format!("{}.json", listing.commit));
        if path.exists() || listing.files.values().any(|b| b.size.is_none() || b.blob_id.is_none()) {
            return;
        }
        let mut files = serde_json::Map::new();
        for (name, blob) in &listing.files {
            let mut info = serde_json::Map::new();
            info.insert("size".into(), json!(blob.size));
            info.insert("blob_id".into(), json!(blob.blob_id));
            if let Some(sha) = &blob.lfs_sha256 {
                info.insert("lfs_sha256".into(), json!(sha));
                info.insert("lfs_size".into(), json!(blob.lfs_size));
            }
            files.insert(name.clone(), Value::Object(info));
        }
        let data = pyjson::indent_one(&json!({"format_version": 1, "files": files}));
        let _ = write_atomically(&path, data.as_bytes());
    }

    fn resolve_url(&self, repo_id: &str, commit: &str, name: &str) -> String {
        format!("{}/{repo_id}/resolve/{commit}/{}", self.endpoint, quote(name, true))
    }
}

/// One file of a Hub listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Blob {
    pub size: Option<u64>,
    pub blob_id: Option<String>,
    pub lfs_sha256: Option<String>,
    pub lfs_size: Option<u64>,
}

impl Blob {
    /// Its name in blobs/: the LFS SHA-256, else the git blob SHA-1.
    pub fn etag(&self) -> Option<&str> {
        self.lfs_sha256.as_deref().or(self.blob_id.as_deref())
    }
}

pub struct Listing {
    pub commit: String,
    pub files: BTreeMap<String, Blob>,
}

/// Write a small file by rename, so readers never see it partly written.
fn write_atomically(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().expect("a parent");
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".{}.{}.tmp", path.file_name().unwrap().to_string_lossy(), rand::random::<u32>()));
    let result = (|| {
        let mut file = File::create(&staged)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&staged, path)
    })();
    let _ = fs::remove_file(&staged);
    result
}

/// An exclusive flock, released on drop: the same lock huggingface_hub's filelock takes on the same path.
pub struct FileLock(File);

impl FileLock {
    /// Take the lock, calling `waiting` once if another process holds it.
    pub fn acquire(path: &Path, waiting: impl FnOnce()) -> io::Result<FileLock> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).read(true).open(path)?;
        // SAFETY: flock on an open descriptor owned by `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            waiting();
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(FileLock(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // SAFETY: as above; closing would release it too.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// A local file read through a read-only map, so a header read touches only the header's pages.
pub struct Mapped {
    map: memmap2::Mmap,
    position: usize,
}

impl Mapped {
    pub fn open(path: &Path) -> Result<Mapped> {
        let file = File::open(path)?;
        // SAFETY: read-only; a file truncated while mapped faults, as the native reader's map would.
        Ok(Mapped { map: unsafe { memmap2::Mmap::map(&file)? }, position: 0 })
    }
}

impl Read for Mapped {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let rest = &self.map[self.position.min(self.map.len())..];
        let n = rest.len().min(buf.len());
        buf[..n].copy_from_slice(&rest[..n]);
        self.position += n;
        Ok(n)
    }
}

/// A Hub file read on demand by range requests: a GGUF header costs a few requests, not a download.
struct RangeReader<'a> {
    hub: &'a Hub,
    url: String,
    position: u64,
    block: Vec<u8>,
    block_start: u64,
}

impl Read for RangeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let end = self.block_start + self.block.len() as u64;
        if self.position < self.block_start || self.position >= end {
            let range = format!("bytes={}-{}", self.position, self.position + RANGE_BLOCK - 1);
            let request = self.hub.get(&self.url).map_err(io::Error::other)?.header("Range", range);
            let response = self.hub.send(request).map_err(io::Error::other)?;
            match response.status().as_u16() {
                416 => return Ok(0),
                206 => {}
                status => {
                    return Err(io::Error::other(Error::Hub(format!("the Hub ignored a range request ({status})"))))
                }
            }
            self.block = response.bytes().map_err(|e| io::Error::other(Error::Hub(self.hub.reason(e, None))))?.to_vec();
            self.block_start = self.position;
            if self.block.is_empty() {
                return Ok(0);
            }
        }
        let offset = (self.position - self.block_start) as usize;
        let n = (self.block.len() - offset).min(buf.len());
        buf[..n].copy_from_slice(&self.block[offset..offset + n]);
        self.position += n as u64;
        Ok(n)
    }
}

/// One source at one commit (hub.Repository): listed by the Hub, read from that commit's cached snapshot, or local
/// files (no revision).
#[derive(Clone, Debug)]
pub struct Repository {
    pub name: String,
    pub revision: Option<String>,
    pub files: BTreeSet<String>,
    /// Each file's local path, for a cached snapshot or local files.
    pub paths: Option<BTreeMap<String, PathBuf>>,
    /// Hub filename -> its listing, to name its blob and report what a download fetches.
    pub sizes: BTreeMap<String, Blob>,
    /// Why the Hub did not resolve this source, when its cached snapshot or installed commit stands in.
    pub unreachable: Option<String>,
}

fn files_under(directory: &Path) -> BTreeMap<String, PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, PathBuf>) {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            // As rglob: a real folder is walked; a link to one is not followed (a cycle would never end).
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                walk(root, &path, out);
            } else if path.is_file() {
                let name = path.strip_prefix(root).expect("under root").to_string_lossy().into_owned();
                out.insert(name, path);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(directory, directory, &mut out);
    out
}

impl Repository {
    fn new(name: &str, revision: Option<String>) -> Repository {
        Repository {
            name: name.into(),
            revision,
            files: BTreeSet::new(),
            paths: None,
            sizes: BTreeMap::new(),
            unreachable: None,
        }
    }

    /// A commit known without listing it: a verified installation's, whose links name its files.
    pub fn unlisted(name: &str, commit: Option<&str>, unreachable: Option<String>) -> Repository {
        Repository { unreachable, ..Repository::new(name, commit.map(str::to_string)) }
    }

    pub fn local_directory(path: &Path) -> Result<Repository> {
        if !path.is_dir() {
            return Err(Error::model(format!("local draft directory not found: {}", path.display())));
        }
        let paths = files_under(path);
        Ok(Repository {
            files: paths.keys().cloned().collect(),
            paths: Some(paths),
            ..Repository::new(&path.to_string_lossy(), None)
        })
    }

    /// name at commit from its snapshot in the Hub cache, without the Hub: only files downloaded before.
    pub fn cached(hub: &Hub, name: &str, commit: &str, unreachable: Option<String>) -> Result<Repository> {
        let path = hub.snapshot(name, commit);
        if !path.is_dir() {
            return Err(Error::model(format!("the Hub cache has no snapshot {commit} of {name}")));
        }
        let paths = files_under(&path);
        Ok(Repository {
            files: paths.keys().cloned().collect(),
            paths: Some(paths),
            sizes: hub.read_tree(name, commit).unwrap_or_default(),
            unreachable,
            ..Repository::new(name, Some(commit.into()))
        })
    }

    /// A source as an assembly record names it, without the Hub.
    pub fn recorded(hub: &Hub, repo: &str, revision: Option<&str>) -> Result<Repository> {
        match revision {
            None => Repository::local_directory(Path::new(repo)),
            Some(commit) => Repository::cached(hub, repo, commit, None),
        }
    }

    /// name at the commit `revision` names now (hub.Repository.resolve). With `installed` (a verified installation's
    /// commit), offline or a commit revision returns it without a request; else one request resolves the revision,
    /// and when the Hub cannot answer the installed commit, or a cached snapshot of a commit this selection already
    /// names, stands in. `unreachable` stands for the Hub's answer for another repository this start.
    pub fn resolve(
        hub: &Hub,
        name: &str,
        revision: Option<&str>,
        installation: Option<&Path>,
        installed: Option<&str>,
        unreachable: Option<String>,
    ) -> Result<Repository> {
        if Path::new(name).is_absolute() {
            return Repository::local_directory(Path::new(name));
        }
        super::check_repo_id(name)?;
        let commit_revision = revision.is_some_and(|r| is_hex(r, 40));
        if let Some(installed) = installed {
            if hub.offline || commit_revision {
                return Ok(Repository::unlisted(name, Some(installed), None));
            }
        }
        let why = if hub.offline {
            "HF_HUB_OFFLINE is set".to_string()
        } else if let Some(why) = unreachable {
            why
        } else {
            match hub.model_info(name, revision) {
                Ok(listing) => {
                    return Ok(Repository {
                        files: listing.files.keys().cloned().collect(),
                        sizes: listing.files,
                        ..Repository::new(name, Some(listing.commit))
                    })
                }
                Err(error) => error.to_string(),
            }
        };
        if let Some(installed) = installed {
            return Ok(Repository::unlisted(name, Some(installed), Some(why)));
        }
        let commits = cached_commits(hub, name, revision, installation);
        for commit in &commits {
            if hub.snapshot(name, commit).is_dir() {
                return Repository::cached(hub, name, commit, Some(why));
            }
        }
        let branch = revision.unwrap_or("the default branch");
        let cache = if !commits.is_empty() {
            format!("the Hub cache has no snapshot of {}", commits.join(", "))
        } else if installation.is_some() {
            format!("neither this installation nor the Hub cache records a commit for {branch}")
        } else {
            format!("the Hub cache records no commit for {branch}")
        };
        Err(Error::model(format!("cannot resolve {name}: {why}; {cache}")))
    }

    fn require(&self, name: &str) -> Result<()> {
        if !self.files.contains(name) {
            return Err(Error::model(format!("missing {name} in {}", self.name)));
        }
        if !is_safe_path(name) {
            return Err(Error::model(format!("unsupported file name in {}: {name}", self.name)));
        }
        Ok(())
    }

    fn commit(&self) -> &str {
        self.revision.as_deref().unwrap_or_default()
    }

    /// The file's path if the cache (or local directory) holds it.
    pub fn local_path(&self, hub: &Hub, name: &str) -> Option<PathBuf> {
        if let Some(paths) = &self.paths {
            return paths.get(name).cloned();
        }
        let pointer = hub.snapshot(&self.name, self.commit()).join(name);
        pointer.exists().then_some(pointer)
    }

    /// Whether the cache holds the file's content, at this commit or as a blob another commit shares.
    pub fn present(&self, hub: &Hub, name: &str) -> bool {
        self.local_path(hub, name).is_some()
            || self.sizes.get(name).and_then(Blob::etag).is_some_and(|e| hub.folder(&self.name).join("blobs").join(e).exists())
    }

    /// The file's size: listed, or the cached file's.
    pub fn size(&self, hub: &Hub, name: &str) -> Option<u64> {
        self.sizes
            .get(name)
            .and_then(|b| b.size)
            .or_else(|| self.local_path(hub, name).and_then(|p| fs::metadata(p).ok()).map(|m| m.len()))
    }

    /// A small file's bytes (a config, an index), from the cache or one request, without writing anything.
    pub fn read(&self, hub: &Hub, name: &str) -> Result<Vec<u8>> {
        self.require(name)?;
        const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;
        if let Some(path) = self.local_path(hub, name) {
            if fs::metadata(&path)?.len() > MAX_JSON_BYTES {
                return Err(Error::model(format!("JSON metadata is too large: {}", path.display())));
            }
            return Ok(fs::read(path)?);
        }
        let url = hub.resolve_url(&self.name, self.commit(), name);
        let response = hub.send(hub.get(&url)?)?;
        let mut data = Vec::new();
        response.take(MAX_JSON_BYTES + 1).read_to_end(&mut data).map_err(|e| Error::Hub(hub.reason(e, None)))?;
        if data.len() as u64 > MAX_JSON_BYTES {
            return Err(Error::model(format!("JSON metadata is too large: {name} in {}", self.name)));
        }
        Ok(data)
    }

    /// A stream of the file read on demand: mapped when cached, else by range requests.
    pub fn open<'a>(&self, hub: &'a Hub, name: &str) -> Result<Box<dyn Read + 'a>> {
        self.require(name)?;
        if let Some(path) = self.local_path(hub, name) {
            return Ok(Box::new(Mapped::open(&path)?));
        }
        let url = hub.resolve_url(&self.name, self.commit(), name);
        Ok(Box::new(RangeReader { hub, url, position: 0, block: Vec::new(), block_start: 0 }))
    }

    /// Each of names -> its path in the snapshot, downloading what the cache lacks (hub.Repository.download).
    pub fn download(
        &self,
        hub: &Hub,
        names: &BTreeSet<String>,
        progress: &dyn Fn(&Event),
    ) -> Result<BTreeMap<String, PathBuf>> {
        for name in names {
            self.require(name)?;
        }
        if let Some(paths) = &self.paths {
            return Ok(names.iter().map(|n| (n.clone(), paths[n].clone())).collect());
        }
        let blobs = hub.folder(&self.name).join("blobs");
        let fetch: Vec<u64> = names
            .iter()
            .filter(|n| self.local_path(hub, n).is_none())
            .filter_map(|n| self.sizes.get(n))
            .filter(|b| !b.etag().is_some_and(|e| blobs.join(e).exists()))
            .filter_map(|b| b.size)
            .collect();
        if !fetch.is_empty() {
            progress(&Event::Status(format!(
                "Fetching {} file(s), {:.2} GB, from {}@{}; cached files are reused.",
                fetch.len(),
                fetch.iter().sum::<u64>() as f64 / 1e9,
                self.name,
                &self.commit()[..12]
            )));
        }
        let listing = Listing { commit: self.commit().into(), files: self.sizes.clone() };
        if !self.sizes.is_empty() {
            hub.write_tree(&self.name, &listing);
        }
        let mut out = BTreeMap::new();
        for name in names {
            out.insert(name.clone(), self.download_one(hub, name, progress)?);
        }
        Ok(out)
    }

    /// One file into the cache (file_download._hf_hub_download_to_cache_dir): the snapshot's link if present, else
    /// the blob if another commit fetched it, else a download into `<blob>.incomplete`, resumed where an earlier one
    /// stopped, checked against its digest, then published by rename and linked from the snapshot.
    fn download_one(&self, hub: &Hub, name: &str, progress: &dyn Fn(&Event)) -> Result<PathBuf> {
        let folder = hub.folder(&self.name);
        let pointer = folder.join("snapshots").join(self.commit()).join(name);
        if pointer.exists() {
            return Ok(pointer);
        }
        let blob = self.sizes.get(name).filter(|b| b.etag().is_some() && b.size.is_some()).ok_or_else(|| {
            Error::model(format!("the Hub did not list {name} in {}; it cannot be fetched offline", self.name))
        })?;
        let etag = blob.etag().expect("checked").to_string();
        if !(is_hex(&etag, 40) || is_hex(&etag, 64)) {
            return Err(Error::model(format!("the Hub listed {name} with an unknown digest: {etag}")));
        }
        let size = blob.size.expect("checked");
        let blob_path = folder.join("blobs").join(&etag);
        let lock = hub.cache.join(".locks").join(folder_name(&self.name)).join(format!("{etag}.lock"));
        let _lock = FileLock::acquire(&lock, || {
            progress(&Event::Status(format!("Waiting for another download of {name}...")))
        })?;
        if !blob_path.exists() {
            self.fetch(hub, name, &blob_path, &etag, size, progress)?;
        }
        if !pointer.exists() {
            link_pointer(&pointer, &etag, name)?;
        }
        Ok(pointer)
    }

    fn fetch(
        &self,
        hub: &Hub,
        name: &str,
        blob_path: &Path,
        etag: &str,
        size: u64,
        progress: &dyn Fn(&Event),
    ) -> Result<()> {
        fs::create_dir_all(blob_path.parent().expect("blobs"))?;
        let incomplete = PathBuf::from(format!("{}.incomplete", blob_path.display()));
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(&incomplete)?;
        let mut done = file.metadata()?.len();
        if done > size {
            file.set_len(0)?;
            done = 0;
        }
        let report = |done: u64| {
            progress(&Event::Download { repo: self.name.clone(), file: name.into(), done, total: size })
        };
        if done < size {
            let url = hub.resolve_url(&self.name, self.commit(), name);
            let mut request = hub.get(&url)?;
            if done > 0 {
                request = request.header("Range", format!("bytes={done}-"));
            }
            let mut response = hub.send(request)?;
            if response.status().as_u16() == 200 && done > 0 {
                // The server sent the whole file: start over.
                file.set_len(0)?;
                done = 0;
            }
            let mut buffer = vec![0u8; 1 << 20];
            let mut reported = done;
            report(done);
            loop {
                let n = response.read(&mut buffer).map_err(|e| Error::Hub(hub.reason(e, None)))?;
                if n == 0 {
                    break;
                }
                file.write_all(&buffer[..n])?;
                done += n as u64;
                if done - reported >= 16 << 20 || done == size {
                    report(done);
                    reported = done;
                }
            }
            file.sync_all()?;
        }
        if done != size {
            return Err(Error::Hub(format!(
                "download of {name} from {} stopped at {done} of {size} bytes; preparing again resumes it",
                self.name
            )));
        }
        // The digest the Hub published for the file, over the whole file, resumed parts included.
        let digest = if etag.len() == 64 { sha256_file(&incomplete)? } else { git_sha1_file(&incomplete)? };
        if !digest.eq_ignore_ascii_case(etag) {
            fs::remove_file(&incomplete)?;
            return Err(Error::model(format!("{name} from {} does not match its published digest", self.name)));
        }
        fs::rename(&incomplete, blob_path)?;
        report(size);
        Ok(())
    }

    pub fn identity(&self) -> Value {
        json!({"repo": self.name, "revision": self.revision})
    }
}

/// snapshots/<commit>/<name> -> ../../blobs/<etag>, relative as huggingface_hub links it.
fn link_pointer(pointer: &Path, etag: &str, name: &str) -> Result<()> {
    fs::create_dir_all(pointer.parent().expect("snapshot"))?;
    let up = "../".repeat(name.matches('/').count() + 2);
    let target = format!("{up}blobs/{etag}");
    let staged = pointer.with_file_name(format!(".{}.{}.link", etag, rand::random::<u32>()));
    std::os::unix::fs::symlink(&target, &staged)?;
    fs::rename(&staged, pointer).inspect_err(|_| {
        let _ = fs::remove_file(&staged);
    })?;
    Ok(())
}

/// The commits of name whose cached snapshots may stand in for revision without the Hub, most specific first:
/// revision itself when it is a commit; the installation's recorded commit, then its pins; then the cache's ref.
fn cached_commits(hub: &Hub, name: &str, revision: Option<&str>, installation: Option<&Path>) -> Vec<String> {
    if let Some(r) = revision.filter(|r| is_hex(r, 40)) {
        return vec![r.to_lowercase()];
    }
    let mut commits = Vec::new();
    if let Some(installation) = installation {
        let record: Option<Value> =
            fs::read(installation.join("model.json")).ok().and_then(|d| serde_json::from_slice(&d).ok());
        if let Some(sources) = record.as_ref().and_then(|r| r["sources"].as_object()) {
            for source in sources.values() {
                if source["repo"] == json!(name) {
                    if let Some(c) = source["revision"].as_str() {
                        commits.push(c.to_string());
                    }
                }
            }
        }
        let pins = hub.folder(name).join("refs").join("splash").join(pin_owner(installation));
        let mut refs: Vec<(i128, String)> = fs::read_dir(pins)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                let ns = modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos() as i128;
                Some((-ns, e.file_name().to_string_lossy().into_owned()))
            })
            .collect();
        refs.sort();
        commits.extend(refs.into_iter().map(|(_, n)| n));
    }
    if let Ok(commit) = fs::read_to_string(hub.folder(name).join("refs").join(revision.unwrap_or("main"))) {
        commits.push(commit.trim().to_string());
    }
    let mut seen = BTreeSet::new();
    commits.into_iter().filter(|c| is_hex(c, 40) && seen.insert(c.clone())).collect()
}

/// The refs/splash folder holding an installation's pins: the SHA-256 of its path with a resolved parent.
pub fn pin_owner(installation: &Path) -> String {
    let parent = installation.parent().unwrap_or(Path::new("/"));
    let parent = parent.canonicalize().unwrap_or_else(|_| parent.to_path_buf());
    let owner = parent.join(installation.file_name().unwrap_or_default());
    sha256_hex(owner.as_os_str().as_bytes())
}

/// installation's pin of a snapshot: refs/splash/<installation>/<commit> in its repository's folder.
pub fn pinned(snapshot: &Path, installation: &Path) -> PathBuf {
    let repository = snapshot.parent().and_then(Path::parent).expect("a snapshot in a Hub cache");
    repository.join("refs").join("splash").join(pin_owner(installation)).join(snapshot.file_name().expect("commit"))
}

/// Pin a snapshot for an installation, so pruning the Hub cache cannot remove files the installation links.
pub fn pin(snapshot: &Path, repo_id: &str, installation: &Path) -> Result<PathBuf> {
    let commit = snapshot.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let in_cache = snapshot.parent().and_then(Path::file_name).is_some_and(|n| n == "snapshots")
        && snapshot.parent().and_then(Path::parent).and_then(Path::file_name).is_some_and(|n| *n == *folder_name(repo_id))
        && is_hex(&commit, 40);
    if !in_cache {
        return Err(Error::model(format!("{} is not a snapshot of {repo_id} in a Hub cache", snapshot.display())));
    }
    let reference = pinned(snapshot, installation);
    match fs::read_to_string(&reference) {
        Ok(existing) if existing == commit => return Ok(reference),
        Ok(_) => {
            return Err(Error::model(format!("invalid installed snapshot reference: {}", reference.display())))
        }
        Err(_) => {}
    }
    write_atomically(&reference, commit.as_bytes())?;
    Ok(reference)
}

/// Remove the installation's pins beside `pins` and beside `replaced`, once the installation they pin is published.
pub fn retire_other_pins(pins: &[PathBuf], replaced: &[PathBuf], progress: &dyn Fn(&Event)) {
    let folders: BTreeSet<&Path> = pins.iter().chain(replaced).filter_map(|r| r.parent()).collect();
    for folder in folders {
        for entry in fs::read_dir(folder).into_iter().flatten().flatten() {
            let path = entry.path();
            if !pins.contains(&path) && is_hex(&entry.file_name().to_string_lossy(), 40) {
                if let Err(error) = fs::remove_file(&path) {
                    progress(&Event::Status(format!("Warning: could not retire old Hub cache references: {error}")));
                }
            }
        }
    }
}

/// Pin a verified installation's snapshots again and retire its older pins; a read-only cache only warns.
pub fn repair_pins(installation: &Path, snapshots: &[(PathBuf, String)], progress: &dyn Fn(&Event)) -> Result<()> {
    let mut pins = Vec::new();
    for (path, repo_id) in snapshots {
        match pin(path, repo_id, installation) {
            Ok(reference) => pins.push(reference),
            Err(Error::Io(e)) if matches!(e.raw_os_error(), Some(libc::EACCES | libc::EPERM | libc::EROFS)) => {
                progress(&Event::Status(format!(
                    "Warning: the verified model can be used, but its Hub cache reference could not be retained: {e}"
                )));
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    }
    retire_other_pins(&pins, &[], progress);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_blob_ids_match_git() {
        let dir = std::env::temp_dir().join(format!("hub-sha1-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        fs::write(&path, b"hello\n").unwrap();
        // `printf 'hello\n' | git hash-object --stdin`
        assert_eq!(git_sha1_file(&path).unwrap(), "ce013625030ba8dba906f756967f9e9ca394464a");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn urls_quote_as_huggingface_hub() {
        assert_eq!(quote("a b/c+d.gguf", true), "a%20b/c%2Bd.gguf");
        assert_eq!(quote("refs/pr/1", false), "refs%2Fpr%2F1");
    }
}
