//! `heimdall-splash serve` and `heimdall-splash pull` (install/launcher.py and server.py main, without Python).
//!
//! `serve` prepares the model (downloading what the Hugging Face cache lacks), starts the native engine, waits for
//! it to load, and serves HTTP. A crashed engine is relaunched as Splash's backend relaunches it (`Recovery`); when
//! that stops, a crash loop, the server exits and Heimdall's engine manager takes over. SIGTERM/SIGINT stop the
//! engine first. Status lines go to stderr as Splash's did (`Loading ·`, `Ready ·`).
use super::app::App;
use super::http::{router, Server};
use super::options::ServeOptions;
use crate::chat::template::ChatTemplates;
use crate::chat::tokens::PromptTokenizer;
use crate::constrain::ConstraintFactory;
use crate::engine::{Engine, EngineOptions};
use crate::hub::{self, PrepareOptions, Progress};
use anyhow::{bail, ensure, Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long the engine may take to load (server.NATIVE_START_TIMEOUT); a first start calibrates.
const NATIVE_START_TIMEOUT: Duration = Duration::from_secs(600);
/// A failure counts toward a crash loop unless its engine served this long (backend.CRASH_LOOP_WINDOW_SECONDS).
const CRASH_LOOP_WINDOW: Duration = Duration::from_secs(60);
/// Consecutive such failures, failed relaunches included, that stop relaunching.
const CRASH_LOOP_LIMIT: u32 = 3;
/// The n-th consecutive relaunch waits RESTART_BACKOFF * 2^(n-2); the first waits nothing.
const RESTART_BACKOFF: Duration = Duration::from_secs(5);
/// What a stop leaves the engine to flush its SSD cache; Heimdall's manager waits 30 s.
const STOP_GRACE: Duration = Duration::from_secs(25);

pub fn status(line: &str) {
    let now = chrono_like_time();
    eprintln!("{now} {line}");
}

fn chrono_like_time() -> String {
    // Local wall time as HH:MM:SS, as Splash's console lines carry it.
    let t: libc::time_t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as libc::time_t);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

pub fn main(args: Vec<OsString>) -> Result<()> {
    let (command, rest) = args.split_first().context("usage: heimdall-splash serve --model OWNER/REPO[:VARIANT] [options] | pull OWNER/REPO[:VARIANT]")?;
    match command.to_str() {
        Some("serve") => serve(ServeOptions::parse(rest)?),
        Some("pull") => pull(rest, false),
        Some("inspect") => pull(rest, true),
        Some("--version") => {
            println!("heimdall-splash {} (Splash 1.4.1 engine protocol 11)", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        _ => bail!("unknown command; use serve or pull"),
    }
}

/// Heimdall keeps models beside the engine's cache: `<data>/splash/models` for `<data>/splash/prefix-cache`.
pub fn models_dir(options: &ServeOptions) -> PathBuf {
    options.models_dir.clone().unwrap_or_else(|| match &options.cache_dir {
        Some(cache) => cache.parent().map_or_else(|| cache.join("models"), |p| p.join("models")),
        None => heimdall_home::data_dir().join("splash/models"),
    })
}

/// The native engine: `--binary`, else `$SPLASH_ENGINE`, else the app bundle's `Resources/splash/engine/splash`.
pub fn engine_binary(options: &ServeOptions) -> Result<PathBuf> {
    if let Some(binary) = &options.binary {
        return Ok(binary.clone());
    }
    if let Some(env) = std::env::var_os("SPLASH_ENGINE") {
        return Ok(env.into());
    }
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("executable directory")?;
    let bundled = dir.join("../Resources/splash/engine/splash");
    ensure!(bundled.is_file(), "the native engine is not bundled beside {} (pass --binary)", exe.display());
    // A clean path in the process list, which Heimdall's engine manager matches.
    Ok(bundled.canonicalize().unwrap_or(bundled))
}

fn progress() -> Progress {
    let last = std::sync::Mutex::new((String::new(), 0u64));
    Arc::new(move |event: &hub::Event| match event {
        hub::Event::Status(line) => status(line),
        hub::Event::Download { repo, file, done, total } => {
            // One line per file per 5% keeps the log readable through a 16 GB download.
            let pct = if *total > 0 { done * 100 / total } else { 0 };
            let mut last = last.lock().unwrap();
            let key = format!("{repo}/{file}");
            if last.0 != key || pct >= last.1 + 5 || done == total {
                *last = (key, pct);
                status(&format!("Downloading · {repo} · {file} · {pct}% of {:.1} GB", *total as f64 / 1e9));
            }
        }
    })
}

/// A prepared model directory: the selector's, or `--model` naming a directory already prepared.
fn prepare(options: &ServeOptions, engine: &Path) -> Result<(PathBuf, &'static crate::chat::format::Family)> {
    let path = Path::new(&options.model);
    if path.is_dir() {
        let record: serde_json::Value = serde_json::from_slice(&std::fs::read(path.join("model.json")).context("model.json of the model directory")?)?;
        let family = record["family"].as_str().and_then(crate::chat::format::family).context("the model directory's family is not one this release serves")?;
        return Ok((path.to_path_buf(), family));
    }
    let mut prepare = PrepareOptions::new(models_dir(options));
    prepare.offline = options.offline;
    prepare.language_only = options.language_only;
    prepare.engine = Some(engine.to_path_buf());
    prepare.progress = Some(progress());
    let prepared = hub::prepare(&options.model, &prepare).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((prepared.root, prepared.family))
}

fn serve(options: ServeOptions) -> Result<()> {
    let binary = engine_binary(&options)?;
    // Bind before loading so a second server fails at once; serve only once the engine is ready.
    let listener = std::net::TcpListener::bind((options.host.as_str(), options.port)).with_context(|| format!("unable to start HTTP server on {}:{}", options.host, options.port))?;
    status(&format!("Loading · {}", options.model));
    let (root, family) = prepare(&options, &binary)?;
    let tokenizer = Arc::new(PromptTokenizer::from_dir(&root.join("tokenizer")).context("tokenizer")?);
    tokenizer.validate(&family.chat_format).map_err(|e| anyhow::anyhow!("{e}"))?;
    let templates = ChatTemplates::load(&root.join("tokenizer"), tokenizer.encoder()).map_err(|e| anyhow::anyhow!("{e}"))?;
    status(&format!("Chat template · {}", templates.describe()));
    let constraints = ConstraintFactory::new(tokenizer.tokenizer(), family.chat_format)?;
    let command = options.native_command(&binary, &root);
    let engine_options = EngineOptions { startup_timeout: NATIVE_START_TIMEOUT, pending_limit: options.queue_size, ..EngineOptions::default() };
    let engine = Engine::spawn_with(command.clone(), engine_options.clone())?;
    let ready = engine.wait_ready(Some(NATIVE_START_TIMEOUT)).context("native runtime did not become ready")?;
    ensure!(
        match options.max_context {
            Some(c) => ready.max_context_tokens == c,
            None => (1..=super::options::MAX_CONTEXT_TOKENS).contains(&ready.max_context_tokens),
        },
        "native runtime reported an invalid context window"
    );
    let response_model = if options.announce_served_name { options.served_model_names[0].clone() } else { options.model.clone() };
    let mut model_names = vec![response_model.clone()];
    for name in std::iter::once(&options.model).chain(&options.served_model_names) {
        if !model_names.contains(name) {
            model_names.push(name.clone());
        }
    }
    let app = Arc::new(App {
        engine: super::app::EngineSlot::new(engine),
        tokenizer,
        templates,
        format: family.chat_format,
        constraints,
        images: crate::images::ImageCache::new(),
        ready,
        model: options.model.clone(),
        response_model,
        model_names,
        default_reasoning_effort: options.default_reasoning_effort.clone(),
        default_thinking_budget: options.thinking_token_budget,
        request_timeout: options.request_timeout,
        max_image_pixels: crate::images::MAX_PIXELS,
        started_at: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()),
    });
    let context = ready.max_context_tokens;
    let context = if context % 1024 == 0 { format!("{}K", context / 1024) } else { context.to_string() };
    let mode = if ready.vision { "" } else { " · language only" };
    status(&format!("Ready · {} · context {context}{mode} · http://{}:{}", options.model, options.host, options.port));
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;
    let server = Server::new(app.clone(), options.queue_size, &options.host, options.port);
    // A relaunch keeps the first engine's context, which its own start would calibrate anew.
    let mut relaunch = command.clone();
    if options.max_context.is_none() {
        relaunch.extend(["--relaunch-context".into(), ready.max_context_tokens.to_string().into()]);
    }
    let mut recovery = Recovery::start(app.clone(), relaunch, engine_options);
    let recovery_reason = recovery.stopped.clone();
    let result = runtime.block_on(async {
        listener.set_nonblocking(true)?;
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let stopped = recovery_reason.clone();
        let engine_down = async move {
            loop {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if let Some(reason) = stopped.lock().unwrap().clone() {
                    return reason;
                }
            }
        };
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let serve = axum::serve(listener, router(server, options.max_request_bytes));
        tokio::select! {
            r = serve => r.map_err(anyhow::Error::from),
            reason = engine_down => Err(anyhow::anyhow!("the inference engine stopped: {reason}")),
            _ = term.recv() => Ok(()),
            _ = tokio::signal::ctrl_c() => Ok(()),
        }
    });
    status("Stopping · releasing engine resources");
    recovery.stop();
    app.engine.get().shutdown(STOP_GRACE);
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Relaunches a failed engine (backend.NativeBackend._failed, _restart_delay_locked): at once the first time, then
/// after a doubling backoff; a failure of an engine that served a full window starts the count again. Three in a
/// row stop it, and the server exits with the reason.
struct Recovery {
    stopped: Arc<std::sync::Mutex<Option<String>>>,
    /// Set when the server stops: an engine it shut down is no crash, and nothing may start after it.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Recovery {
    fn start(app: Arc<App>, command: Vec<OsString>, options: EngineOptions) -> Self {
        let stopped = Arc::new(std::sync::Mutex::new(None));
        let stopping = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (out, flag) = (stopped.clone(), stopping.clone());
        let thread = std::thread::Builder::new()
            .name("splash-recovery".into())
            .spawn(move || {
                if let Some(reason) = Self::run(&app, &command, options, &flag) {
                    *out.lock().unwrap() = Some(reason);
                }
            })
            .expect("recovery thread");
        Recovery { stopped, stopping, thread: Some(thread) }
    }

    /// Stop relaunching and wait for the thread: an engine it was starting is shut down, never left running.
    fn stop(&mut self) {
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    /// The reason relaunching stopped; None when the server stopped it.
    fn run(app: &App, command: &[OsString], options: EngineOptions, stopping: &std::sync::atomic::AtomicBool) -> Option<String> {
        let stopping = || stopping.load(std::sync::atomic::Ordering::SeqCst);
        let first = app.ready;
        let mut ready_at = std::time::Instant::now();
        let mut failures = 0u32;
        loop {
            if stopping() {
                return None;
            }
            let engine = app.engine.get();
            let Some(error) = engine.failure() else {
                std::thread::sleep(Duration::from_millis(200));
                continue;
            };
            let mut reason = error.to_string();
            let mut served = ready_at.elapsed();
            // Stop and reap the failed engine before another loads, as runtime.py does: two would hold the weights
            // and the cache at once.
            engine.shutdown(Duration::from_secs(15));
            drop(engine);
            loop {
                if served >= CRASH_LOOP_WINDOW {
                    failures = 0;
                }
                failures += 1;
                if failures >= CRASH_LOOP_LIMIT {
                    return Some(format!("the inference engine failed {failures} times in a row, each within 60 s of starting ({reason})"));
                }
                let delay = if failures == 1 { Duration::ZERO } else { RESTART_BACKOFF * 2u32.pow(failures - 2) };
                status(&format!("Engine failed · {reason} · relaunching in {:.0} s", delay.as_secs_f64()));
                let until = std::time::Instant::now() + delay;
                while std::time::Instant::now() < until {
                    if stopping() {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                if stopping() {
                    return None;
                }
                let relaunched = Engine::spawn_with(command.to_vec(), options.clone()).map_err(|e| e.to_string()).and_then(|engine| {
                    // Waited in slices, so a server stopping meanwhile stops this engine at once (the engine's own
                    // startup timeout bounds the wait).
                    let ready = loop {
                        match engine.wait_ready(Some(Duration::from_millis(200))) {
                            Ok(ready) => break ready,
                            Err(crate::engine::EngineError::Timeout(_)) if !stopping() && engine.failure().is_none() => continue,
                            Err(crate::engine::EngineError::Timeout(_)) => {
                                engine.shutdown(Duration::from_secs(5));
                                return Err("STOPPING".into());
                            }
                            Err(e) => {
                                engine.shutdown(Duration::from_secs(5));
                                return Err(engine.failure().map_or(e.to_string(), |f| f.to_string()));
                            }
                        }
                    };
                    // The server serves the first engine's limits; one that announces others would at every relaunch.
                    if (ready.max_context_tokens, ready.max_concurrent_requests, ready.vision) != (first.max_context_tokens, first.max_concurrent_requests, first.vision) {
                        engine.shutdown(STOP_GRACE);
                        return Err("FATAL native context window, concurrency or vision changed".into());
                    }
                    Ok(engine)
                });
                if stopping() || relaunched.as_ref().is_err_and(|e| e == "STOPPING") {
                    // The server stopped while this engine loaded: it must not outlive the server.
                    if let Ok(engine) = relaunched {
                        engine.shutdown(STOP_GRACE);
                    }
                    return None;
                }
                match relaunched {
                    Ok(engine) => {
                        // The engine it replaces was stopped and reaped above.
                        app.engine.replace(engine);
                        ready_at = std::time::Instant::now();
                        status("Ready · the engine is serving again");
                        break;
                    }
                    Err(e) if e.starts_with("FATAL ") => return Some(e["FATAL ".len()..].to_string()),
                    Err(e) => {
                        reason = e;
                        served = Duration::ZERO;
                    }
                }
            }
        }
    }
}

/// Prepare a model without serving it (what the setup downloads), or with `inspect` only say, as JSON, what it holds
/// and what a download would cost.
fn pull(args: &[OsString], inspect_only: bool) -> Result<()> {
    let mut selector = None;
    let mut options = ServeOptions::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.to_str() {
            Some("--models-dir") => options.models_dir = Some(PathBuf::from(it.next().context("--models-dir needs a value")?)),
            Some("--offline") => options.offline = true,
            Some("--language-only") => options.language_only = true,
            Some("--binary") => options.binary = Some(PathBuf::from(it.next().context("--binary needs a value")?)),
            Some(s) if !s.starts_with("--") && selector.is_none() => selector = Some(s.to_string()),
            _ => bail!("usage: heimdall-splash pull OWNER/REPO[:VARIANT] [--models-dir D] [--offline] [--language-only]"),
        }
    }
    options.model = selector.context("pull needs a model, such as unsloth/Qwen3.8-27B-GGUF:UD-Q4_K_M")?;
    let binary = engine_binary(&options).ok();
    let mut prepare = PrepareOptions::new(models_dir(&options));
    prepare.offline = options.offline;
    prepare.language_only = options.language_only;
    prepare.engine = binary;
    if inspect_only {
        let plan = hub::inspect(&options.model, &prepare).map_err(|e| anyhow::anyhow!("{e}"))?;
        let files: Vec<_> = plan.files.iter().map(|f| serde_json::json!({"repo": f.repo, "file": f.file, "bytes": f.bytes, "present": f.present})).collect();
        println!("{}", serde_json::json!({"model": plan.model, "family": plan.family.name, "installed": plan.installed, "present_bytes": plan.present_bytes, "missing_bytes": plan.missing_bytes, "unreachable": plan.unreachable, "files": files}));
        return Ok(());
    }
    prepare.progress = Some(progress());
    let prepared = hub::prepare(&options.model, &prepare).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{}", prepared.root.display());
    Ok(())
}
