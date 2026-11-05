//! First-run model downloads (public edition; docs/models.md, "First-run downloads"). The public app bundles only the
//! small models; Parakeet, the speaker models, the decider at the precision chosen in Setup and EmbeddingGemma 2 come
//! from their official Hugging Face repositories at pinned revisions (`model_downloads.lock.json`, written by
//! `scripts/downloads_lock.py`) into `<data>/models`.
//!
//! `<data>/models` is the app's whole model folder in the public edition (`prepare`): every bundled model is linked
//! there from the app (`Contents/Resources/models`), so every loader reads one folder as before, and a downloaded file
//! sits beside them at the path the bundle used.
//!
//! Each file goes to `<path>.part` first: a partial file resumes with an HTTP Range request, the whole file's SHA-256
//! must match the lock, then it is renamed into place (atomic). A failed attempt retries with backoff; a group whose
//! files are all in place is written to `downloads.json` and never fetched again.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The pinned downloads (`scripts/downloads_lock.py`).
pub const LOCK: &str = include_str!("model_downloads.lock.json");
/// Which groups are finished, with the files they were finished with.
pub const MANIFEST: &str = "downloads.json";
pub const PARAKEET: &str = "parakeet";
pub const SPEAKERS: &str = "speakers";
pub const EMBEDDING: &str = "embedding";
/// MossFormer2, the night pass's and speech split's separator (docs/night-pass.md).
pub const NIGHT: &str = "night";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct FileRow {
    /// Relative to the models folder, as the bundle had it.
    pub path: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub title: String,
    pub files: Vec<FileRow>,
}

impl Group {
    pub fn bytes(&self) -> u64 {
        self.files.iter().map(|f| f.bytes).sum()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Lock {
    pub groups: Vec<Group>,
}

impl Lock {
    pub fn embedded() -> Lock {
        serde_json::from_str(LOCK).expect("model_downloads.lock.json")
    }
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.groups.iter().find(|g| g.id == id)
    }
}

/// The decider's group at a precision: "decider-4bit".
pub fn decider_group(p: decide::precision::Precision) -> String {
    format!("decider-{}", p.key())
}

/// What listening needs before it starts: Parakeet, the speaker models and the decider at `p`.
pub fn listening_groups(p: decide::precision::Precision) -> Vec<String> {
    vec![PARAKEET.into(), SPEAKERS.into(), decider_group(p)]
}

/// Everything the public edition downloads for a precision, in the order it downloads: listening first (Parakeet,
/// the speakers, the decider), then the separator and memory search.
pub fn first_run_groups(p: decide::precision::Precision) -> Vec<String> {
    let mut groups = listening_groups(p);
    groups.push(NIGHT.into());
    groups.push(EMBEDDING.into());
    groups
}

// ---- the models folder ----

/// The public edition's models folder, `<data>/models`, with every bundled entry linked from `bundled`. A top-level
/// entry the downloads never write into is one link (`silero` → the app's `silero`); one they share (`decide`,
/// `embeddinggemma2`) is a real folder whose bundled entries are linked one level down. Links are pointed at the app
/// again on every launch (it may have moved or updated); real files are never replaced.
pub fn prepare(bundled: &Path, models: &Path, lock: &Lock) -> Result<()> {
    std::fs::create_dir_all(models).with_context(|| format!("create {}", models.display()))?;
    let downloads: Vec<&str> = lock.groups.iter().flat_map(|g| g.files.iter().map(|f| f.path.as_str())).collect();
    link_level(bundled, models, "", &downloads)
}

fn link_level(bundled: &Path, models: &Path, prefix: &str, downloads: &[&str]) -> Result<()> {
    let from = bundled.join(prefix);
    for entry in std::fs::read_dir(&from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        // The bundle's own manifest describes the bundle; the folder has its own (`MANIFEST`).
        if prefix.is_empty() && (name == "manifest.json" || name == MANIFEST) {
            continue;
        }
        let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let shared = downloads.iter().any(|d| d.starts_with(&format!("{rel}/")) || *d == rel);
        let target = models.join(&rel);
        if shared && entry.file_type()?.is_dir() {
            if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
                std::fs::remove_file(&target)?;
            }
            std::fs::create_dir_all(&target)?;
            link_level(bundled, models, &rel, downloads)?;
            continue;
        }
        if shared {
            // A bundled file the downloads would write (a personal bundle used for a test): the bundle's is the file.
            continue;
        }
        link(&entry.path(), &target)?;
    }
    Ok(())
}

fn link(source: &Path, target: &Path) -> Result<()> {
    match std::fs::symlink_metadata(target) {
        Ok(m) if m.file_type().is_symlink() => {
            if std::fs::read_link(target).ok().as_deref() == Some(source) {
                return Ok(());
            }
            std::fs::remove_file(target)?;
        }
        // Something real is there (put by hand): left alone.
        Ok(_) => return Ok(()),
        Err(_) => {}
    }
    std::os::unix::fs::symlink(source, target).with_context(|| format!("link {}", target.display()))
}

// ---- the manifest ----

#[derive(Default, Deserialize, Serialize)]
struct Manifest {
    #[serde(default)]
    done: BTreeMap<String, Vec<Done>>,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Done {
    path: String,
    sha256: String,
}

impl Manifest {
    fn load(models: &Path) -> Manifest {
        std::fs::read(models.join(MANIFEST)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self, models: &Path) -> Result<()> {
        let path = models.join(MANIFEST);
        let temp = models.join(format!("{MANIFEST}.tmp"));
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }
}

/// A finished group: in the manifest with exactly these files, each still in place at its size. Cheap (no hashing),
/// so it can run on every tick.
pub fn is_done(models: &Path, group: &Group) -> bool {
    let manifest = Manifest::load(models);
    let Some(done) = manifest.done.get(&group.id) else { return false };
    done.len() == group.files.len()
        && group.files.iter().all(|f| done.iter().any(|d| d.path == f.path && d.sha256 == f.sha256))
        && group.files.iter().all(|f| std::fs::metadata(models.join(&f.path)).is_ok_and(|m| m.len() == f.bytes))
}

/// A group that can run now: finished for this lock, or finished for an earlier one (an update that changed a pin) with
/// its files still in place. Listening keeps running on the old files while the new ones download.
pub fn is_usable(models: &Path, group: &Group) -> bool {
    if is_done(models, group) {
        return true;
    }
    Manifest::load(models).done.get(&group.id).is_some_and(|done| !done.is_empty() && done.iter().all(|d| models.join(&d.path).is_file()))
}

fn mark_done(models: &Path, group: &Group) -> Result<()> {
    let mut manifest = Manifest::load(models);
    manifest.done.insert(group.id.clone(), group.files.iter().map(|f| Done { path: f.path.clone(), sha256: f.sha256.clone() }).collect());
    manifest.save(models)
}

// ---- one file ----

#[derive(Clone, Debug)]
pub struct Options {
    /// Failed attempts in a row before a file (and its group) fails. An attempt that brought new bytes resets it.
    pub attempts: u32,
    /// The first retry's wait; each next one doubles, up to a minute.
    pub backoff: Duration,
    /// One request's limit, short so a stalled connection costs at most this: a request that runs out keeps its
    /// bytes, and the next one resumes from them at once.
    pub request_timeout: Duration,
    /// A failed group is tried again by itself after this, doubling each time, up to 30 minutes (`retry_due`).
    pub retry_after: Duration,
    /// Tests: every URL's `https://huggingface.co` (and the other hosts) replaced by this origin.
    pub origin: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self { attempts: 6, backoff: Duration::from_secs(2), request_timeout: Duration::from_secs(90), retry_after: Duration::from_secs(120), origin: mirror(std::env::var(MIRROR_ENV).ok()) }
    }
}

/// Test runs only (docs/models.md, "Test mirror"): a cache server that serves the same paths as Hugging Face, e.g.
/// `HEIMDALL_MODEL_MIRROR=http://192.168.64.1:8800` for the VM. Every file is still checked against its pinned
/// SHA-256, so a mirror can make downloads faster but never different. Unset (every normal run): Hugging Face.
pub const MIRROR_ENV: &str = "HEIMDALL_MODEL_MIRROR";

/// The mirror's origin from the variable's value: an `http(s)://host[:port]` with no path; anything else is ignored.
pub fn mirror(value: Option<String>) -> Option<String> {
    let v = value?.trim().trim_end_matches('/').to_string();
    let rest = v.strip_prefix("http://").or_else(|| v.strip_prefix("https://"))?;
    (!rest.is_empty() && !rest.contains('/')).then(|| {
        crate::diagnostics::line("downloads", &format!("test mirror {v} (every file still checked against its SHA-256)"));
        v
    })
}

impl Options {
    fn url(&self, url: &str) -> String {
        match &self.origin {
            Some(origin) => match url.split_once("://").and_then(|(_, rest)| rest.split_once('/')) {
                Some((_, path)) => format!("{origin}/{path}"),
                None => url.to_string(),
            },
            None => url.to_string(),
        }
    }
}

/// Why a file did not land.
#[derive(Debug)]
enum Attempt {
    /// Try again (the network, the server, a short body, a request that ran out of time).
    Retry(String),
    /// Stop: nothing will change by trying (cancelled, the disk, a file that does not match its checksum: fetching
    /// gigabytes again would bring the same bytes).
    Stop(String),
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

fn sha256_file(path: &Path, hasher: &mut Sha256) -> std::io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut n = 0;
    loop {
        let read = f.read(&mut buf)?;
        if read == 0 {
            return Ok(n);
        }
        hasher.update(&buf[..read]);
        n += read as u64;
    }
}

/// One file into place: resumed from its `.part`, hashed, renamed. `progress` gets this file's bytes on disk.
pub fn fetch_file(client: &reqwest::blocking::Client, models: &Path, file: &FileRow, options: &Options, progress: &mut dyn FnMut(u64), cancelled: &dyn Fn() -> bool) -> Result<()> {
    let target = models.join(&file.path);
    if std::fs::metadata(&target).is_ok_and(|m| m.len() == file.bytes) {
        let mut h = Sha256::new();
        if sha256_file(&target, &mut h).is_ok() && hex(&h.finalize()) == file.sha256 {
            progress(file.bytes);
            return Ok(());
        }
        // An older pin's file stays in use until the new one is renamed over it.
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let part = part_path(&target);
    let mut failures = 0u32;
    let mut wait = options.backoff;
    loop {
        if cancelled() {
            bail!("cancelled");
        }
        let before = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        match attempt(client, file, &part, &target, options, progress, cancelled) {
            Ok(()) => return Ok(()),
            Err(Attempt::Stop(why)) => bail!("{why}"),
            Err(Attempt::Retry(why)) => {
                let after = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                // A request that moved the file forward by a megabyte or more was not a failure in a row.
                if after >= before + (1 << 20) {
                    failures = 0;
                    wait = options.backoff;
                }
                failures += 1;
                crate::diagnostics::line("downloads", &format!("{}: {why} (attempt {failures} of {})", file.path, options.attempts));
                if failures >= options.attempts {
                    bail!("{why}");
                }
                let until = Instant::now() + wait;
                while Instant::now() < until {
                    if cancelled() {
                        bail!("cancelled");
                    }
                    std::thread::sleep(Duration::from_millis(50).min(wait));
                }
                wait = (wait * 2).min(Duration::from_secs(60));
            }
        }
    }
}

fn attempt(client: &reqwest::blocking::Client, file: &FileRow, part: &Path, target: &Path, options: &Options, progress: &mut dyn FnMut(u64), cancelled: &dyn Fn() -> bool) -> std::result::Result<(), Attempt> {
    let retry = |e: &dyn std::fmt::Display| Attempt::Retry(e.to_string());
    let disk = |e: std::io::Error| Attempt::Stop(format!("Could not write {}: {e}", part.display()));
    let mut have = std::fs::metadata(part).map(|m| m.len()).unwrap_or(0);
    if have > file.bytes {
        std::fs::remove_file(part).map_err(disk)?;
        have = 0;
    }
    if have < file.bytes {
        let mut request = client.get(options.url(&file.url)).timeout(options.request_timeout);
        if have > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut response = request.send().map_err(|e| retry(&format!("{e}")))?;
        let status = response.status();
        let resumed = status == reqwest::StatusCode::PARTIAL_CONTENT;
        if resumed {
            // The range must start where the file ends.
            let start = response.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("bytes ")).and_then(|v| v.split('-').next()).and_then(|v| v.parse::<u64>().ok());
            if start != Some(have) {
                std::fs::remove_file(part).ok();
                return Err(Attempt::Retry(format!("the server resumed at {start:?}, not {have}")));
            }
        } else if status.is_success() {
            // A whole file: whatever was there goes.
            have = 0;
        } else if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            std::fs::remove_file(part).ok();
            return Err(Attempt::Retry("the partial file did not fit the server's".into()));
        } else {
            return Err(Attempt::Retry(format!("HTTP {status}")));
        }
        let mut out = std::fs::OpenOptions::new().create(true).write(true).append(resumed).truncate(!resumed).open(part).map_err(disk)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if cancelled() {
                return Err(Attempt::Stop("cancelled".into()));
            }
            let n = response.read(&mut buf).map_err(|e| retry(&format!("the connection dropped: {e}")))?;
            if n == 0 {
                break;
            }
            if have + n as u64 > file.bytes {
                drop(out);
                std::fs::remove_file(part).ok();
                return Err(Attempt::Retry("the server sent more than the file's size".into()));
            }
            out.write_all(&buf[..n]).map_err(disk)?;
            have += n as u64;
            progress(have);
        }
        out.sync_all().map_err(disk)?;
        if have < file.bytes {
            return Err(Attempt::Retry(format!("the connection ended at {have} of {} bytes", file.bytes)));
        }
    }
    // The whole file as it is on disk, its size and its SHA-256, right before it goes into place.
    let mut hasher = Sha256::new();
    let on_disk = sha256_file(part, &mut hasher).map_err(disk)?;
    if on_disk != file.bytes || hex(&hasher.finalize()) != file.sha256 {
        std::fs::remove_file(part).ok();
        return Err(Attempt::Stop(CHECKSUM.into()));
    }
    std::fs::rename(part, target).map_err(disk)?;
    progress(file.bytes);
    Ok(())
}

/// A file whose bytes are not the pinned ones fails at once (no gigabytes fetched again for the same answer).
pub const CHECKSUM: &str = "the download does not match its checksum";

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---- the downloader ----

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Downloading,
    Done,
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub id: String,
    pub title: String,
    pub done: u64,
    pub total: u64,
    pub state: State,
}

struct Inner {
    rows: Vec<Row>,
    running: bool,
    /// Per failed group: when it failed and how many times it was retried by itself (`retry_due`).
    failed: std::collections::HashMap<String, (Instant, u32)>,
}

/// The public edition's downloads: groups asked for (`want`) download one file at a time on one thread, in the order
/// asked; `rows` is what Setup shows. `changed` runs (on the download thread) whenever a row moves, at most every
/// 200 ms while bytes arrive.
pub struct Downloads {
    models: PathBuf,
    lock: Lock,
    options: Options,
    inner: Mutex<Inner>,
    changed: Box<dyn Fn() + Send + Sync>,
    stop: std::sync::atomic::AtomicBool,
}

impl Downloads {
    pub fn new(models: PathBuf, lock: Lock, options: Options, changed: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self { models, lock, options, inner: Mutex::new(Inner { rows: vec![], running: false, failed: Default::default() }), changed: Box::new(changed), stop: Default::default() })
    }

    pub fn models(&self) -> &Path {
        &self.models
    }

    pub fn lock(&self) -> &Lock {
        &self.lock
    }

    /// The group is finished on disk (the manifest; no network).
    pub fn ready(&self, id: &str) -> bool {
        self.lock.group(id).is_some_and(|g| is_done(&self.models, g))
    }

    /// Adds these groups (unknown ids are ignored) to the list, finished ones as done, and starts the thread when
    /// something is left. A failed group asked for again is tried again.
    pub fn want(self: &Arc<Self>, ids: &[String]) {
        {
            let mut inner = self.inner.lock().unwrap();
            for id in ids {
                let Some(group) = self.lock.group(id) else { continue };
                let done = is_done(&self.models, group);
                if let Some(row) = inner.rows.iter_mut().find(|r| &r.id == id) {
                    if matches!(row.state, State::Failed(_)) {
                        row.state = if done { State::Done } else { State::Waiting };
                    }
                    continue;
                }
                let at = listening_end(&inner.rows, id);
                inner.rows.insert(at, Row {
                    id: group.id.clone(),
                    title: group.title.clone(),
                    done: if done { group.bytes() } else { 0 },
                    total: group.bytes(),
                    state: if done { State::Done } else { State::Waiting },
                });
            }
        }
        (self.changed)();
        self.run();
    }

    /// Can run now (`is_usable`): what listening waits for.
    pub fn usable(&self, id: &str) -> bool {
        self.lock.group(id).is_some_and(|g| is_usable(&self.models, g))
    }

    /// The decider at another precision: a decider group still waiting (or failed) that is not `chosen` leaves the list,
    /// and `chosen` downloads next, before the models listening does not need. One already downloading finishes (its
    /// bytes would be lost otherwise).
    pub fn choose_decider(self: &Arc<Self>, chosen: &str) {
        self.inner.lock().unwrap().rows.retain(|r| !(r.id.starts_with("decider-") && r.id != chosen && matches!(r.state, State::Waiting | State::Failed(_))));
        self.want(&[chosen.to_string()]);
    }

    /// The settings tick: a failed group whose wait is over is tried again (two minutes, doubling to 30), and groups
    /// left waiting (another Heimdall held the folder's lock) start when they can. Never hot: a failure in a row only
    /// lengthens the wait.
    pub fn tick(self: &Arc<Self>) {
        let due: Vec<String> = {
            let mut inner = self.inner.lock().unwrap();
            let Inner { rows, failed, .. } = &mut *inner;
            let mut due = vec![];
            // A checksum mismatch stays failed: fetching again brings the same bytes (a bad pin, a corrupting proxy), so
            // only the user's Retry tries it (review 5 Nov: whole files again every 2-30 minutes, forever).
            for row in rows.iter_mut().filter(|r| matches!(&r.state, State::Failed(why) if !why.contains(CHECKSUM))) {
                let (at, tries) = *failed.entry(row.id.clone()).or_insert((Instant::now(), 0));
                if at.elapsed() >= self.retry_wait(tries) {
                    failed.insert(row.id.clone(), (at, tries + 1));
                    row.state = State::Waiting;
                    due.push(row.id.clone());
                }
            }
            due
        };
        if !due.is_empty() {
            crate::diagnostics::line("downloads", &format!("trying again: {}", due.join(", ")));
            (self.changed)();
        }
        self.run();
    }

    fn retry_wait(&self, tries: u32) -> Duration {
        (self.options.retry_after * 2u32.saturating_pow(tries.min(8))).min(Duration::from_secs(30 * 60))
    }

    /// How long until a failed group is tried again by itself.
    pub fn retry_in(&self, id: &str) -> Option<Duration> {
        let inner = self.inner.lock().unwrap();
        if inner.rows.iter().any(|r| r.id == id && matches!(&r.state, State::Failed(why) if why.contains(CHECKSUM))) { return None; }
        let (at, tries) = *inner.failed.get(id)?;
        Some(self.retry_wait(tries).saturating_sub(at.elapsed()))
    }

    /// Retry: every failed group again, from where it stopped.
    pub fn retry(self: &Arc<Self>) {
        let failed: Vec<String> = self.inner.lock().unwrap().rows.iter().filter(|r| matches!(r.state, State::Failed(_))).map(|r| r.id.clone()).collect();
        self.want(&failed);
    }

    pub fn rows(&self) -> Vec<Row> {
        self.inner.lock().unwrap().rows.clone()
    }

    /// Stops the thread at its next chunk (Quit); partial files stay for the next launch.
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }

    fn run(self: &Arc<Self>) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.running || !inner.rows.iter().any(|r| r.state == State::Waiting) {
                return;
            }
            inner.running = true;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().name("heimdall-model-downloads".into()).spawn(move || me.work());
        if spawned.is_err() {
            self.inner.lock().unwrap().running = false;
        }
    }

    fn set(&self, id: &str, f: impl FnOnce(&mut Row)) {
        if let Some(row) = self.inner.lock().unwrap().rows.iter_mut().find(|r| r.id == id) {
            f(row);
        }
        (self.changed)();
    }

    fn work(self: Arc<Self>) {
        // One process downloads into a folder at a time (two launches, an update's relaunch): the other leaves its
        // groups waiting and tries again on a later tick.
        let Some(_held) = folder_lock(&self.models) else {
            crate::diagnostics::line("downloads", "another Heimdall is downloading into this folder; waiting");
            self.inner.lock().unwrap().running = false;
            return;
        };
        let client = reqwest::blocking::Client::builder().connect_timeout(Duration::from_secs(30)).timeout(self.options.request_timeout).build();
        loop {
            let next = {
                let mut inner = self.inner.lock().unwrap();
                match inner.rows.iter().find(|r| r.state == State::Waiting).map(|r| r.id.clone()) {
                    Some(id) => Some(id),
                    None => {
                        inner.running = false;
                        None
                    }
                }
            };
            let Some(id) = next else { return };
            let Some(group) = self.lock.group(&id).cloned() else { continue };
            let result = match &client {
                Ok(client) => self.fetch_group(client, &group),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            };
            match result {
                Ok(()) => {
                    crate::diagnostics::line("downloads", &format!("{} ready ({} bytes)", group.id, group.bytes()));
                    self.inner.lock().unwrap().failed.remove(&id);
                    self.set(&id, |r| { r.state = State::Done; r.done = r.total; });
                }
                Err(e) => {
                    let why = format!("{e:#}");
                    crate::diagnostics::line("downloads", &format!("{} failed: {why}", group.id));
                    // The wait before the next try counts from now; the tries made so far are kept.
                    { let mut inner = self.inner.lock().unwrap(); let tries = inner.failed.get(&id).map_or(0, |f| f.1); inner.failed.insert(id.clone(), (Instant::now(), tries)); }
                    self.set(&id, |r| r.state = State::Failed(why));
                    if self.stop.load(std::sync::atomic::Ordering::Acquire) {
                        self.inner.lock().unwrap().running = false;
                        return;
                    }
                }
            }
        }
    }

    fn fetch_group(&self, client: &reqwest::blocking::Client, group: &Group) -> Result<()> {
        if is_done(&self.models, group) {
            return Ok(());
        }
        self.set(&group.id, |r| r.state = State::Downloading);
        let cancelled = || self.stop.load(std::sync::atomic::Ordering::Acquire);
        let mut before = 0u64;
        let mut shown = Instant::now() - Duration::from_secs(1);
        for file in &group.files {
            let mut progress = |n: u64| {
                if shown.elapsed() >= Duration::from_millis(200) || n == file.bytes {
                    shown = Instant::now();
                    self.set(&group.id, |r| r.done = before + n);
                }
            };
            fetch_file(client, &self.models, file, &self.options, &mut progress, &cancelled).with_context(|| file.path.rsplit('/').next().unwrap_or_default().to_string())?;
            before += file.bytes;
        }
        mark_done(&self.models, group)
    }
}

/// The folder's download lock (`<models>/.downloads.lock`, an advisory `flock`), held while the thread works; `None`
/// when another process holds it.
fn folder_lock(models: &Path) -> Option<std::fs::File> {
    use std::os::fd::AsRawFd;
    std::fs::create_dir_all(models).ok()?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(models.join(".downloads.lock")).ok()?;
    (unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0).then_some(file)
}

/// Where a new row goes: a group listening needs goes after the last one of those (so it downloads before memory
/// search and the separator); any other at the end.
fn listening_end(rows: &[Row], id: &str) -> usize {
    let listening = |id: &str| id == PARAKEET || id == SPEAKERS || id.starts_with("decider-");
    if !listening(id) {
        return rows.len();
    }
    rows.iter().rposition(|r| listening(&r.id)).map_or(0, |i| i + 1)
}

// ---- the app's one instance ----

static GLOBAL: std::sync::OnceLock<Arc<Downloads>> = std::sync::OnceLock::new();

/// The public edition's launch: `<data>/models` made from the bundle (`prepare`) and the downloads set up, every
/// change sent to Setup. Returns the folder the app loads its models from. Nothing downloads until asked
/// (`Pipeline::start_model_downloads`).
pub fn open(bundled: &Path, data: &Path, ui: Option<ui::Ui>) -> Result<PathBuf> {
    let models = data.join("models");
    let lock = Lock::embedded();
    prepare(bundled, &models, &lock)?;
    let downloads = Downloads::new(models.clone(), lock, Options::default(), move || {
        if let (Some(ui), Some(d)) = (ui, global()) {
            ui.send(ui::UiUpdate::ModelDownloads(view(&d)));
        }
    });
    let _ = GLOBAL.set(downloads);
    Ok(models)
}

static UNAVAILABLE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The public edition could not make `<data>/models` (`open` failed): listening stays off and says why.
pub fn set_unavailable(reason: String) {
    let _ = UNAVAILABLE.set(reason);
}

pub fn unavailable() -> Option<&'static str> {
    UNAVAILABLE.get().map(String::as_str)
}

/// The downloads, in the public edition's app only (tests and the personal edition: none).
pub fn global() -> Option<Arc<Downloads>> {
    GLOBAL.get().cloned()
}

/// What Setup's Getting ready shows: a row per group asked for, in download order.
pub fn view(d: &Downloads) -> ui::ModelDownloads {
    ui::ModelDownloads {
        rows: d.rows().into_iter().map(|r| {
            let precision = r.id.strip_prefix("decider-").and_then(decide::precision::Precision::from_key);
            ui::ModelDownload {
                symbol: match r.id.as_str() {
                    PARAKEET => "waveform",
                    SPEAKERS => "person.2.wave.2",
                    EMBEDDING => "magnifyingglass",
                    NIGHT => "arrow.triangle.branch",
                    _ => "target",
                }.into(),
                title: r.title,
                detail: precision.map(|p| p.label().to_string()),
                done: r.done,
                total: r.total,
                state: match r.state {
                    State::Waiting => ui::DownloadState::Waiting,
                    State::Downloading => ui::DownloadState::Downloading,
                    State::Done => ui::DownloadState::Done,
                    State::Failed(why) => ui::DownloadState::Failed(short(&why)),
                },
            }
        }).collect(),
    }
}

/// A failure in a few words for the row: the network named plainly, else the first clause.
pub(crate) fn short(why: &str) -> String {
    let lower = why.to_lowercase();
    if lower.contains("dns") || lower.contains("connect") || lower.contains("timed out") || lower.contains("connection") {
        return "No connection to Hugging Face".into();
    }
    if lower.contains("checksum") {
        return "The download did not match its checksum".into();
    }
    if lower.contains("could not write") {
        return "Could not write to the disk".into();
    }
    let line = why.lines().next().unwrap_or(why);
    if line.chars().count() > 48 { format!("{}\u{2026}", line.chars().take(47).collect::<String>()) } else { line.to_string() }
}

#[cfg(test)]
#[path = "model_store_tests.rs"]
mod tests;
