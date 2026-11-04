//! Splash ownership. All process/HTTP work stays off the capture loop. The script and
//! app share both the JSON serve configuration and an advisory lifecycle lock.
use anyhow::{ensure, Context, Result};
use crossbeam_channel::{Receiver, Sender};
use serde::Deserialize;
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    net::{SocketAddr, TcpStream},
    os::unix::{io::AsRawFd, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use ui::{EngineState, HealthControl, Part, UiUpdate};

const DEFAULTS: &str = include_str!("../config/inference-engine.json");
const KEY: &str = "inference-engine";

/// One precision (quantization) of a model: the Splash spec it serves and, from the catalog, its download and the
/// memory it needs. `ram_bytes` is derived, not measured: GGUF weights + vision projector (if any) + DFlash2 draft +
/// int8 KV cache bytes per token x the model's context (262,144) + 1 GB (1e9) runtime overhead
/// (Assistant/Plans/heimdall-engine-models/catalog.json, `ram_formula` in config/inference-engine.json).
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Quant {
    pub quant: String,
    pub source: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub ram_bytes: Option<u64>,
}
/// A model the engine can serve. `quant`/`source` are its default precision; `quants` every precision offered. An
/// override in the older shape (`quant` and `source` only, no `quants`) reads as a model with that one precision.
#[derive(Clone, Debug, Deserialize)]
pub struct Model {
    pub id: String,
    pub label: String,
    pub quant: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub context: Option<u64>,
    #[serde(default)]
    pub quants: Vec<Quant>,
}
impl Model {
    pub fn quant(&self, quant: &str) -> Option<&Quant> { self.quants.iter().find(|q| q.quant == quant) }
    /// The precision the user chose (`choice`), else the default when he chose none or one no longer offered.
    pub fn chosen(&self, choice: Option<&str>) -> &Quant {
        choice.and_then(|c| self.quant(c)).or_else(|| self.quant(&self.quant)).unwrap_or(&self.quants[0])
    }
}
#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    executable: PathBuf,
    pub port: u16,
    max_memory: String,
    max_cache_disk: String,
    pub default_model: String,
    pub models: Vec<Model>,
}
/// `owner/repo[:VARIANT]` as Splash accepts it (crates/splash/src/hub/mod.rs `split_model_id`): it names the
/// selection link `<models root>/owner/repo:VARIANT`, so it must stay one safe relative path.
pub fn check_source(source: &str) -> Result<()> {
    let (repo, variant) = match source.split_once(':') { Some((r, v)) => (r, Some(v)), None => (source, None) };
    let part = |s: &str, max: usize| {
        let edge = |c: char| c.is_ascii_alphanumeric() || c == '_';
        !s.is_empty() && s.len() <= max && s.chars().next().is_some_and(edge) && s.chars().last().is_some_and(edge)
            && s.chars().all(|c| edge(c) || c == '.' || c == '-')
    };
    let repo_ok = repo.split_once('/').is_some_and(|(o, r)| part(o, 96) && part(r, 96))
        && !repo.contains("--") && !repo.contains("..") && !repo.ends_with(".git");
    let variant_ok = variant.is_none_or(|v| (1..=64).contains(&v.len()) && v.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !v.contains(".."));
    ensure!(repo_ok && variant_ok, "invalid engine model source {source:?}");
    Ok(())
}
impl Config {
    pub fn bundled() -> Self {
        let mut config: Self = serde_json::from_str(DEFAULTS).expect("bundled engine config");
        config.normalize();
        config
    }
    /// The older shape (one `source` per model) gains its one precision; a model without a `source` takes its
    /// default precision's.
    fn normalize(&mut self) {
        for m in &mut self.models {
            if m.quants.is_empty() && !m.source.is_empty() {
                m.quants.push(Quant { quant: m.quant.clone(), source: m.source.clone(), size_bytes: None, ram_bytes: None });
            }
            if let Some(q) = m.quants.iter().find(|q| q.quant == m.quant) { m.source = q.source.clone(); }
            m.quants.sort_by_key(|q| q.size_bytes.unwrap_or(0));
        }
    }
    pub fn load(data: &Path) -> Result<Self> {
        let path = data.join("splash/engine.json");
        let mut config: Self = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("engine configuration")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::bundled(),
            Err(e) => return Err(e.into()),
        };
        config.normalize();
        ensure!(
            (config.executable.is_absolute()
                || matches!(config.executable.components().collect::<Vec<_>>()[..], [std::path::Component::Normal(_)]))
                && config.port != 0,
            "invalid engine executable/port"
        );
        ensure!(!config.models.is_empty(), "empty engine model list");
        config.model(&config.default_model)?;
        let mut ids = std::collections::HashSet::new();
        for model in &config.models {
            ensure!(
                ids.insert(&model.id) && !model.id.is_empty() && !model.quants.is_empty() && model.quant(&model.quant).is_some(),
                "invalid engine model list"
            );
            let mut quants = std::collections::HashSet::new();
            for q in &model.quants {
                ensure!(quants.insert(&q.quant) && !q.quant.is_empty(), "invalid engine model precisions");
                check_source(&q.source)?;
            }
        }
        // Preserve the resource ceilings: editing the model list cannot lift them.
        ensure!(
            config.max_memory == "48G" && config.max_cache_disk == "100G",
            "engine resource ceilings must stay 48G/100G"
        );
        Ok(config)
    }
    /// The configuration the app runs: `<data>/splash/engine.json` when it is valid, else the bundled default.
    /// The second value says why an override was set aside (for the log); Heimdall never asks the user to edit it.
    pub fn effective(data: &Path) -> (Self, Option<String>) {
        match Self::load(data) {
            Ok(config) => (config, None),
            Err(e) => (
                Self::bundled(),
                Some(format!(
                    "{} is unusable ({e:#}); using the bundled default",
                    data.join("splash/engine.json").display()
                )),
            ),
        }
    }
    /// The server to start. A bare name is a binary beside Heimdall's own (`Contents/MacOS/heimdall-splash`, the
    /// bundled Rust server, docs/splash-native.md); an absolute path is used as given (Homebrew's `splash`, say).
    pub fn executable(&self) -> PathBuf {
        if self.executable.is_absolute() {
            return self.executable.clone();
        }
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join(&self.executable)))
            .unwrap_or_else(|| self.executable.clone())
    }
    pub fn model(&self, id: &str) -> Result<&Model> {
        self.models
            .iter()
            .find(|m| m.id == id)
            .context("engine model unavailable")
    }
    /// The precision `quant` of `model`; an unknown precision is an error.
    pub fn quant(&self, model: &str, quant: &str) -> Result<&Quant> {
        self.model(model)?.quant(quant).context("engine model precision unavailable")
    }
    /// The engine's memory ceiling in bytes (`max_memory`, 48G: 48 x 2^30).
    pub fn memory_limit_bytes(&self) -> Option<u64> {
        self.max_memory.strip_suffix('G').and_then(|n| n.parse::<u64>().ok()).map(|g| g << 30)
    }
    fn args(&self, model: &str, quant: &str, cache: &Path) -> Result<Vec<String>> {
        let m = self.model(model)?;
        Ok(vec![
            "serve".into(),
            "--model".into(),
            self.quant(model, quant)?.source.clone(),
            "--port".into(),
            self.port.to_string(),
            "--served-model-name".into(),
            m.id.clone(),
            "--persistent-cache".into(),
            "--max-cache-disk".into(),
            self.max_cache_disk.clone(),
            "--cache-dir".into(),
            cache.display().to_string(),
            "--max-memory".into(),
            self.max_memory.clone(),
        ])
    }
    pub fn update(
        &self,
        model: &str,
        state: EngineState,
        status: Option<&Value>,
        models: Option<&Value>,
    ) -> UiUpdate {
        let label = self
            .model(model)
            .map(|m| m.label.clone())
            .unwrap_or_else(|_| model.into());
        UiUpdate::Engine {
            state,
            model_id: model.into(),
            model_label: label,
            models: self
                .models
                .iter()
                .map(|m| self.ui_model(m, None, &|_| false, None))
                .collect(),
            port: self.port,
            context_limit: models
                .and_then(|v| v["data"].as_array())
                .and_then(|a| a.iter().find(|m| m["id"].as_str() == Some(model)))
                .and_then(|m| m["max_model_len"].as_u64()),
            // Splash reports the SSD cache's bytes under a top-level `disk` (`/status`, 1.3.1 to 1.4.1).
            cache_used_bytes: status.and_then(|v| v["disk"]["used_bytes"].as_u64()),
            cache_limit_bytes: status.and_then(|v| v["disk"]["capacity_bytes"].as_u64()),
            memory_limit_bytes: self.memory_limit_bytes(),
        }
    }
    /// The menu's row for a model: its chosen precision (`choice`, else the default), every precision with its size,
    /// memory and whether it is on this Mac (`installed(source)`), and a download in progress.
    pub fn ui_model(&self, m: &Model, choice: Option<&str>, installed: &dyn Fn(&str) -> bool, download: Option<ui::EngineDownload>) -> ui::EngineModel {
        let chosen = m.chosen(choice);
        ui::EngineModel {
            id: m.id.clone(),
            label: m.label.clone(),
            quant: chosen.quant.clone(),
            source: chosen.source.clone(),
            quants: m.quants.iter().map(|q| ui::EngineQuant {
                quant: q.quant.clone(),
                source: q.source.clone(),
                size_bytes: q.size_bytes,
                ram_bytes: q.ram_bytes,
                downloaded: installed(&q.source),
            }).collect(),
            context: m.context,
            download,
        }
    }
}

#[derive(Clone)]
struct Paths {
    pid: PathBuf,
    cache: PathBuf,
    lock: PathBuf,
    log: PathBuf,
}
impl Paths {
    fn new(data: &Path, log: PathBuf) -> Self {
        let root = data.join("splash");
        Self {
            pid: root.join("splash.pid"),
            cache: root.join("prefix-cache"),
            lock: root.join("lifecycle.lock"),
            log,
        }
    }
}
struct Lock(File);
impl Lock {
    fn acquire(path: &Path) -> Result<Self> {
        crate::settings::private_dir(path.parent().context("lock parent")?)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        // No blocking wait on another owner: report and let an explicit retry acquire it.
        ensure!(
            unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "engine lifecycle is busy"
        );
        Ok(Self(f))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Clone, Debug)]
struct Process {
    pid: i32,
    parent: i32,
    birth: String,
    command: String,
}
fn processes() -> Result<Vec<Process>> {
    let out = Command::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,ppid=,stat=,lstart=,command="])
        .output()?;
    ensure!(out.status.success(), "cannot inspect engine processes");
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 9 || fields[2].starts_with('Z') {
                return None;
            }
            Some(Process {
                pid: fields[0].parse().ok()?,
                parent: fields[1].parse().ok()?,
                birth: fields[3..8].join(" "),
                command: fields[8..].join(" "),
            })
        })
        .collect())
}
// ps flattens argv, including paths with spaces. Match a known value through the
// next option (or end of line), rather than splitting the cache path into words.
fn has_option(command: &str, name: &str, value: &str) -> bool {
    [format!("{name}={value}"), format!("{name} {value}")]
        .iter()
        .any(|needle| {
            command.match_indices(needle).any(|(start, _)| {
                let before = &command[..start];
                let after = &command[start + needle.len()..];
                (before.is_empty() || before.ends_with(char::is_whitespace))
                    && (after.is_empty()
                        || (after.starts_with(char::is_whitespace)
                            && (after.trim_start().is_empty()
                                || after.trim_start().starts_with("--"))))
            })
        })
}
fn owns_cache(p: &Process, paths: &Paths) -> bool {
    let cmd = format!("{} ", p.command);
    let splash = cmd.contains("/splash ")
        || cmd.contains("/splash/")
        || cmd.contains("/launcher.py ")
        || cmd.contains("/heimdall-splash ");
    splash
        && (cmd.contains(" serve ") || cmd.contains(" -m server.server "))
        && has_option(
            &p.command,
            "--cache-dir",
            &paths.cache.display().to_string(),
        )
}
fn native_cache(p: &Process, paths: &Paths) -> bool {
    format!("{} ", p.command).contains("/splash serve-native ")
        && has_option(
            &p.command,
            "--cache-dir",
            &paths.cache.display().to_string(),
        )
}
fn matches_configuration(p: &Process, config: &Config, model: &str, quant: &str) -> Result<bool> {
    Ok(has_option(&p.command, "--port", &config.port.to_string())
        && has_option(&p.command, "--model", &config.quant(model, quant)?.source)
        && has_option(&p.command, "--served-model-name", model))
}
fn same_process(p: &Process, all: &[Process]) -> bool {
    all.iter().any(|q| q.pid == p.pid && q.birth == p.birth)
}
fn signal(p: &Process, sig: i32) -> Result<()> {
    if p.pid > 1 && same_process(p, &processes()?) {
        let rc = unsafe { libc::kill(p.pid, sig) };
        if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
struct Running {
    process: Process,
    child: Option<Child>,
    descendants: Vec<Process>,
}
impl Running {
    fn alive(&mut self) -> Result<bool> {
        let all = processes()?;
        let mut parents = vec![self.process.clone()];
        parents.extend(self.descendants.clone());
        loop {
            let additions: Vec<_> = all
                .iter()
                .filter(|p| {
                    !parents.iter().any(|t| t.pid == p.pid)
                        && parents
                            .iter()
                            .any(|t| t.pid == p.parent && same_process(t, &all))
                })
                .cloned()
                .collect();
            if additions.is_empty() {
                break;
            }
            parents.extend(additions);
        }
        self.descendants = parents
            .into_iter()
            .filter(|p| p.pid != self.process.pid)
            .collect();
        if let Some(child) = &mut self.child {
            if child.try_wait()?.is_some() {
                return Ok(false);
            }
        }
        Ok(same_process(&self.process, &processes()?))
    }
}
fn find_owner(paths: &Paths) -> Result<Option<Process>> {
    owner_in(paths, &processes()?)
}
fn owner_in(paths: &Paths, all: &[Process]) -> Result<Option<Process>> {
    let owners: Vec<_> = all.iter().filter(|p| owns_cache(p, paths)).collect();
    ensure!(
        owners.len() <= 1,
        "multiple engine cache owners; refusing to start"
    );
    ensure!(
        !all.iter()
            .any(|p| native_cache(p, paths) && !owners.iter().any(|owner| owner.pid == p.parent)),
        "orphaned native engine holds the cache; refusing to start another"
    );
    if let Some(owner) = owners.first() {
        return Ok(Some((*owner).clone()));
    }
    if let Ok(raw) = fs::read_to_string(&paths.pid) {
        if let Ok(pid) = raw.trim().parse::<i32>() {
            ensure!(
                !all.iter().any(|p| p.pid == pid),
                "PID file belongs to another process; refusing to signal or overwrite it"
            );
        }
    }
    Ok(None)
}
/// First-launch migration is read-only until Settings persists the adopted intent.
/// The model and precision a verified running engine serves.
pub(crate) fn existing_model(data:&Path)->Result<Option<(String,String)>> {
    let config=Config::effective(data).0;
    let paths=Paths::new(data,PathBuf::new());
    let Some(owner)=find_owner(&paths)? else {return Ok(None)};
    let pid=fs::read_to_string(&paths.pid).ok().and_then(|s|s.trim().parse::<i32>().ok());
    ensure!(pid==Some(owner.pid),"engine PID file does not verify cache owner");
    Ok(config.models.iter().flat_map(|m|m.quants.iter().map(move|q|(m,q)))
        .find(|(m,q)|matches_configuration(&owner,&config,&m.id,&q.quant).unwrap_or(false))
        .map(|(m,q)|(m.id.clone(),q.quant.clone())))
}
fn launch(config: &Config, model: &str, quant: &str, paths: &Paths) -> Result<Running> {
    let _lock = Lock::acquire(&paths.lock)?;
    if let Some(process) = find_owner(paths)? {
        ensure!(
            matches_configuration(&process, config, model, quant)?,
            "running engine configuration differs; turn it off before changing the model"
        );
        crate::settings::private_write(&paths.pid, process.pid.to_string().as_bytes())?;
        return Ok(Running {
            process,
            child: None,
            descendants: vec![],
        });
    }
    // A listener without our verified cache owner must never trigger a second launch.
    ensure!(
        TcpStream::connect_timeout(
            &SocketAddr::from(([127, 0, 0, 1], config.port)),
            Duration::from_millis(200)
        )
        .is_err(),
        "engine port is already occupied"
    );
    crate::settings::private_dir(&paths.cache)?;
    crate::settings::private_dir(paths.log.parent().context("log parent")?)?;
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log)?;
    use std::io::Write;
    writeln!(
        log,
        "=== Heimdall engine start {:?}",
        std::time::SystemTime::now()
    )?;
    let mut command = Command::new(config.executable());
    command
        .args(config.args(model, quant, &paths.cache)?)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().context("start Splash")?;
    let pid = child.id() as i32;
    // Obtain the exact identity after exec, before publishing ownership. Roll back
    // our own new process if PID publication fails; never leave an unowned launcher.
    let ownership = (|| -> Result<Process> {
        let process = processes()?
            .into_iter()
            .find(|p| p.pid == pid)
            .context("engine exited at launch")?;
        crate::settings::private_write(&paths.pid, pid.to_string().as_bytes())?;
        Ok(process)
    })();
    match ownership {
        Ok(process) => Ok(Running {
            process,
            child: Some(child),
            descendants: vec![],
        }),
        Err(e) => {
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while child.try_wait()?.is_none() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(100));
            }
            if child.try_wait()?.is_none() {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                let _ = child.wait();
            }
            Err(e)
        }
    }
}
fn stop(running: &mut Running, paths: &Paths, grace: Duration) -> Result<()> {
    let _lock = Lock::acquire(&paths.lock)?;
    let started = Instant::now();
    let _ = running.alive()?;
    let mut tracked = vec![running.process.clone()];
    tracked.extend(running.descendants.clone());
    let all = processes()?;
    // Retain descendants so a dead launcher cannot orphan the native engine.
    loop {
        let additions: Vec<_> = all
            .iter()
            .filter(|p| {
                !tracked.iter().any(|t| t.pid == p.pid) && tracked.iter().any(|t| t.pid == p.parent)
            })
            .cloned()
            .collect();
        if additions.is_empty() {
            break;
        }
        tracked.extend(additions);
    }
    for p in tracked.iter().rev() { signal(p, libc::SIGTERM)?; }
    loop {
        let _ = running.alive()?; // reap owned zombies and discover late children
        for descendant in &running.descendants {
            if !tracked
                .iter()
                .any(|p| p.pid == descendant.pid && p.birth == descendant.birth)
            {
                signal(descendant, libc::SIGTERM)?;
                tracked.push(descendant.clone());
            }
        }
        let all = processes()?;
        if !tracked.iter().any(|p| same_process(p, &all)) {
            break;
        }
        if started.elapsed() >= grace {
            // Only identities captured from our process tree are signalled.
            for p in tracked.iter().rev() {
                signal(p, libc::SIGKILL)?;
            }
            if let Some(child) = &mut running.child {
                let _ = child.wait();
            }
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    // A concurrent writer/reused PID is never removed.
    if fs::read_to_string(&paths.pid)
        .ok()
        .as_deref()
        .map(str::trim)
        == Some(&running.process.pid.to_string())
    {
        fs::remove_file(&paths.pid)?;
    }
    Ok(())
}
fn get(port: u16, endpoint: &str) -> Result<Value> {
    let out = Command::new("/usr/bin/curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--noproxy",
            "*",
            "--max-time",
            "2",
            "--max-filesize",
            "1048576",
        ])
        .arg(format!("http://127.0.0.1:{port}/{endpoint}"))
        .output()?;
    ensure!(out.status.success(), "engine {endpoint} unavailable");
    Ok(serde_json::from_slice(&out.stdout)?)
}
fn probe(config: &Config) -> Result<(EngineState, Value, Value)> {
    let _ = get(config.port, "health")?;
    let status = get(config.port, "status")?;
    // An explicit released flag, not zero activity/memory, identifies weight release.
    let idle = status["weights"]["released"].as_bool() == Some(true);
    let _ = get(config.port, "ready")?;
    let models = get(config.port, "v1/models")?;
    Ok((
        if idle {
            EngineState::Idle
        } else {
            EngineState::Ready
        },
        status,
        models,
    ))
}

#[derive(Clone, Copy)]
struct Timing {
    starting: Duration,
    ready: Duration,
    startup_limit: Duration,
    stop: Duration,
}
impl Default for Timing {
    fn default() -> Self {
        Self {
            starting: Duration::from_secs(2),
            ready: Duration::from_secs(30),
            startup_limit: Duration::from_secs(1800),
            stop: Duration::from_secs(30),
        }
    }
}
enum Request {
    Startup { on: bool, model: String, quant: String },
    Set { on: bool, model: String, quant: String },
    Quit,
}
pub struct Manager {
    tx: Sender<Request>,
    pub events: Receiver<UiUpdate>,
    worker: Option<JoinHandle<()>>,
}
impl Manager {
    /// `quant`: the precision to serve (one of the model's; an unknown one serves the default).
    pub fn start(data: PathBuf, on: bool, model: String, quant: String, session: PathBuf) -> Self {
        let log = if crate::settings::profile_data().as_ref() == Some(&data) {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join("Library/Logs/Heimdall/splash.log")
        } else {
            data.join("logs/splash.log")
        };
        let paths = Paths::new(&data, log);
        let (tx, rx) = crossbeam_channel::unbounded();
        let (out, events) = crossbeam_channel::unbounded();
        // A broken override never stops the engine: the bundled default runs, and the log says why.
        let worker = thread::spawn(move || {
            let (config, note) = Config::effective(&data);
            if let Some(note) = &note {
                eprintln!("Inference engine configuration: {note}");
            }
            let mut worker = Worker::new(config, paths, out, Timing::default());
            worker.journal = heimdall_archive::records::Writer::open(&session).ok();
            if let Some(note) = note.as_deref() { worker.record_error("configuration-default", note); }
            worker.run(rx)
        });
        let manager = Self {
            tx,
            events,
            worker: Some(worker),
        };
        let _ = manager.tx.send(Request::Startup { on, model, quant });
        manager
    }
    pub fn set(&self, on: bool, model: String, quant: String) {
        let _ = self.tx.send(Request::Set { on, model, quant });
    }
    pub fn request_shutdown(&self) {
        let _ = self.tx.send(Request::Quit);
    }
    pub fn shutdown(&mut self) {
        self.request_shutdown();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.shutdown();
    }
}
struct Worker {
    config: Config,
    paths: Paths,
    out: Sender<UiUpdate>,
    timing: Timing,
    running: Option<Running>,
    model: String,
    quant: String,
    state: EngineState,
    desired: bool,
    started: Instant,
    retries: u8,
    misses: u8,
    health: crate::health::Health,
    last: Option<UiUpdate>,
    journal: Option<heimdall_archive::records::Writer>,
    last_error: Option<String>,
    /// Where this start's lines begin in Splash's log (an engine this worker launched; an adopted one logs elsewhere).
    log_from: Option<u64>,
}

/// A failure as the Engine section shows it: the error's chain in one line, its first letter capitalised, at most
/// 120 characters ("Splash exited", "Engine port is already occupied").
pub(crate) fn failure_reason(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let line = text.lines().next().unwrap_or("").trim();
    let mut chars = line.chars();
    let mut out: String = chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect());
    if out.chars().count() > 120 { out = out.chars().take(119).collect::<String>() + "\u{2026}"; }
    if out.is_empty() { "Inference engine unavailable".into() } else { out }
}

/// How far a start got, from Splash's own status lines (crates/splash/src/server/cli.rs `serve`: `Loading · <model>`,
/// `Downloading · repo · file · N% of X GB` while it fetches what is missing, `Chat template · …`, then `Ready · …`):
/// the last such line wins. `None` before the first: the process is only starting. Splash reports no progress while
/// the weights load into memory, so loading has a percentage only while files download.
pub(crate) fn loading_state(log: &str) -> Option<EngineState> {
    log.lines().rev().find_map(|line| {
        if let Some((_, _, done, total)) = models::parse_progress(line) {
            let percent = if total > 0 { (done * 100 / total).min(100) as u8 } else { 0 };
            return Some(EngineState::Loading { percent: Some(percent) });
        }
        (line.contains(" Loading · ") || line.contains(" Chat template · ") || line.contains(" Ready · ")).then_some(EngineState::Loading { percent: None })
    })
}
impl Worker {
    fn new(config: Config, paths: Paths, out: Sender<UiUpdate>, timing: Timing) -> Self {
        Self {
            model: config.default_model.clone(),
            quant: config.model(&config.default_model).map(|m| m.quant.clone()).unwrap_or_default(),
            config,
            paths,
            out,
            timing,
            running: None,
            state: EngineState::Off,
            desired: false,
            started: Instant::now(),
            retries: 0,
            misses: 0,
            health: crate::health::Health::default(),
            last: None,
            journal: None,
            last_error: None,
            log_from: None,
        }
    }
    fn record_error(&mut self, state: &str, reason: &str) {
        let signature = format!("{state}: {reason}");
        if self.last_error.as_ref() == Some(&signature) { return; }
        self.last_error = Some(signature);
        if let Some(journal) = &self.journal {
            crate::diagnostics::record(journal, "health", "heimdall", serde_json::json!({"component":"inference-engine", "state":state, "reason":reason}));
        }
    }
    fn send(&self, update: UiUpdate) {
        let _ = self.out.send(update);
    }
    fn publish(&mut self, state: EngineState, status: Option<&Value>, models: Option<&Value>) {
        if matches!(state, EngineState::Ready | EngineState::Idle | EngineState::Off) { self.last_error = None; }
        self.state = state.clone();
        let update = self.config.update(&self.model, state, status, models);
        if self.last.as_ref() != Some(&update) {
            self.send(update.clone());
            self.last = Some(update);
        }
    }
    fn fail(&mut self, error: anyhow::Error) {
        use std::io::Write;
        eprintln!("Inference engine: {error:#}");
        self.record_error("failed", &format!("{error:#}"));
        // LaunchServices drops this process's stderr. Keep manager failures (including
        // stop/teardown failures) beside Splash's existing stdout/stderr log.
        let logged = (|| -> Result<()> {
            crate::settings::private_dir(self.paths.log.parent().context("engine log parent")?)?;
            let mut log = OpenOptions::new().create(true).append(true).open(&self.paths.log)?;
            writeln!(log, "Inference engine: {error:#}")?;
            Ok(())
        })();
        if let Err(e) = logged { eprintln!("Could not save inference engine error: {e:#}"); }
        // The reason shows in the menu's Engine section ("failed, with the reason visible"), the
        // error's own words, short.
        self.publish(EngineState::Error(failure_reason(&error)), None, None);
        if let Some(cue) = self.health.problem(
            KEY,
            Part::InferenceEngine,
            "Inference engine unavailable",
            "The menu's Engine line says why",
            HealthControl::OpenMenu,
        ) {
            self.send(UiUpdate::Health(cue));
        }
    }
    fn begin(&mut self) {
        self.publish(EngineState::Starting, None, None);
        self.started = Instant::now();
        self.misses = 0;
        // An explicit retry can re-probe the same process. Preserve its ownership.
        if self.running.as_mut().is_some_and(|r|r.alive().unwrap_or(false)) {return;}
        let from = fs::metadata(&self.paths.log).map(|m| m.len()).unwrap_or(0);
        match launch(&self.config, &self.model, &self.quant, &self.paths) {
            Ok(r) => {
                self.log_from = r.child.is_some().then_some(from);
                self.running = Some(r);
            }
            Err(e) => self.fail(e),
        }
    }
    /// Still starting: Splash's log since this start says whether it is loading the model yet, and how far a download
    /// it needs has come (`loading_state`).
    fn follow_log(&mut self) {
        let Some(from) = self.log_from else { return };
        let Ok(mut file) = File::open(&self.paths.log) else { return };
        use std::io::{Read, Seek, SeekFrom};
        // The tail is enough: Splash writes a handful of status lines per start (a download one per 5% per file).
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let start = from.max(len.saturating_sub(64 * 1024));
        if file.seek(SeekFrom::Start(start)).is_err() { return; }
        let mut text = String::new();
        if file.take(64 * 1024).read_to_string(&mut text).is_err() { return; }
        if let Some(state) = loading_state(&text) {
            if state != self.state { self.publish(state, None, None); }
        }
    }
    /// Starting or loading: no probe answers yet.
    fn starting(&self) -> bool {
        matches!(self.state, EngineState::Starting | EngineState::Loading { .. })
    }
    fn stop_current(&mut self) -> Result<()> {
        if self.running.is_none() {
            let _lock = Lock::acquire(&self.paths.lock)?;
            if let Some(process) = find_owner(&self.paths)? {
                self.running = Some(Running {
                    process,
                    child: None,
                    descendants: vec![],
                });
            }
        }
        if let Some(r) = &mut self.running {
            stop(r, &self.paths, self.timing.stop)?;
        }
        self.running = None;
        Ok(())
    }
    // Saved Off is a startup preference, never an instruction to terminate an owner.
    #[cfg(test)]
    fn startup(&mut self,on:bool,model:String) {
        let quant=self.config.model(&model).map(|m|m.quant.clone()).unwrap_or_default();
        self.startup_quant(on,model,quant);
    }
    fn startup_quant(&mut self,on:bool,model:String,quant:String) {
        self.quant=self.config.model(&model).map(|m|m.chosen(Some(&quant)).quant.clone()).unwrap_or(quant);
        self.model=model;
        self.desired=on;
        if on {self.begin();} else {self.publish(EngineState::Off,None,None);}
    }
    fn shutdown_owned(&mut self) {
        if self.running.as_ref().is_some_and(|r|r.child.is_some()) {
            if let Err(e)=self.stop_current() {self.fail(e);}
        }
    }
    /// `set_quant` on the model's default precision.
    #[cfg(test)]
    fn set(&mut self, on: bool, model: String) {
        let quant = self.config.model(&model).map(|m| m.quant.clone()).unwrap_or_default();
        self.set_quant(on, model, quant);
    }
    fn set_quant(&mut self, on: bool, model: String, quant: String) {
        let quant = match self.config.model(&model) {
            Ok(m) => m.chosen(Some(&quant)).quant.clone(),
            Err(e) => {
                self.fail(e);
                return;
            }
        };
        if on && self.desired && model == self.model && quant == self.quant && !matches!(self.state, EngineState::Error(_))
        {
            return;
        }
        // New explicit intent grants one recovery, never a timer-based restart loop. Another precision is another
        // model: the pipeline turns the engine off before it asks for one (`Pipeline::sync_engine`).
        let changed = self.model != model || self.quant != quant;
        if on && changed && self.running.is_some() {
            self.fail(anyhow::anyhow!("turn the running engine off before changing its model"));
            return;
        }
        // Only turning a desired or owned engine off stops it; a model pick while Off never does.
        let stopping = !on && (self.desired || self.running.is_some());
        self.desired = on;
        if stopping {
            if let Err(e) = self.stop_current() {
                self.fail(e);
                return;
            }
        }
        self.model = model;
        self.quant = quant;
        self.retries = 0;
        if on {
            // Retry keeps the shown fault until a successful probe confirms recovery.
            self.begin();
        } else {
            if let Some(cue) = self.health.clear(KEY) {
                self.send(UiUpdate::Health(cue));
            }
            self.publish(EngineState::Off, None, None);
        }
    }
    fn tick(&mut self) {
        if !self.desired || matches!(self.state, EngineState::Error(_)) {
            return;
        }
        let alive = self.running.as_mut().map(|r| r.alive()).transpose();
        let failure = match alive {
            Ok(Some(true)) => match probe(&self.config) {
                Ok((state, status, models)) => {
                    self.misses = 0;
                    self.publish(state, Some(&status), Some(&models));
                    if let Some(cue) = self.health.recovered(KEY, Part::InferenceEngine, "Inference engine OK") {
                        self.send(UiUpdate::Health(cue));
                    }
                    return;
                }
                Err(_)
                    if self.starting()
                        && self.started.elapsed() < self.timing.startup_limit =>
                {
                    self.follow_log();
                    return
                }
                Err(e) => {
                    self.misses += 1;
                    if self.misses < 2 && !self.starting() {
                        return;
                    }
                    e
                }
            },
            Ok(_) => anyhow::anyhow!("Splash exited"),
            Err(e) => e,
        };
        if self.running.as_ref().is_some_and(|r|r.child.is_none()) {
            // An adopted process belongs to its external launcher. Health only reports.
            self.fail(failure);
            return;
        }
        if let Err(e) = self.stop_current() {
            self.fail(e);
            return;
        }
        if self.retries >= 1 {
            self.fail(failure);
            return;
        }
        self.record_error("restarting", &format!("{failure:#}"));
        self.retries += 1;
        if let Some(cue) = self.health.restarting(KEY, Part::InferenceEngine, "Restarting inference engine…") {
            self.send(UiUpdate::Health(cue));
        }
        self.begin();
    }
    fn run(mut self, rx: Receiver<Request>) {
        self.publish(EngineState::Off, None, None);
        loop {
            let timeout = if self.starting() {
                self.timing.starting
            } else {
                self.timing.ready
            };
            match rx.recv_timeout(timeout) {
                Ok(Request::Startup { on, model, quant }) => self.startup_quant(on, model, quant),
                Ok(Request::Set { on, model, quant }) => self.set_quant(on, model, quant),
                Ok(Request::Quit) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    self.shutdown_owned();
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => self.tick(),
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) { self.shutdown_owned(); }
}

pub mod models;

#[cfg(test)]
pub(crate) mod tests;
#[cfg(test)]
pub(crate) mod models_tests;
