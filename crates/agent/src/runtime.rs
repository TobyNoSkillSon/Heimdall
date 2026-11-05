//! The agent runtime ships inside the signed app: pinned Node, Pi and its production npm dependencies, and
//! Heimdall's own runtime files, in `Contents/Resources/agent-runtime`. It is read-only at run time; nothing is
//! downloaded, copied or repaired at launch. The npm lock pins Pi and every transitive dependency; node-lock.json
//! pins the official Node archive. `heimdall-agent-runtime build` assembles it at build time (scripts/bundle.sh).
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const PACKAGE: &str = include_str!("../runtime/package.json");
pub const LOCK: &str = include_str!("../runtime/package-lock.json");
pub const NODE: &str = include_str!("../runtime/node-lock.json");
const TOOL_OWNER: &str = include_str!("../runtime/tool-owner.cjs");
const LOOP_GUARD: &str = include_str!("../runtime/loop-guard.mjs");
const LOOP_POLICY: &str = include_str!("../runtime/loop-guard.json");
const PROVIDER_RETRY: &str = include_str!("../runtime/provider-retry.mjs");
const THINKING_POLICY: &str = include_str!("../runtime/thinking-policy.mjs");
const HISTORY_REPAIR: &str = include_str!("../runtime/history-repair.mjs");
const SERVICE_TIER: &str = include_str!("../runtime/service-tier.mjs");
const QUOTA: &str = include_str!("../runtime/quota.mjs");
const COMPACTION_FALLBACK: &str = include_str!("../runtime/compaction-fallback.mjs");
const ARSENAL: &str = include_str!("../runtime/arsenal.mjs");
const EXTENSION: &str = include_str!("../runtime/heimdall.ts");
/// Heimdall's own files at the runtime root, beside `node/` and `node_modules/`.
pub const FILES: &[(&str, &str)] = &[
    ("package.json", PACKAGE),
    ("package-lock.json", LOCK),
    ("heimdall.ts", EXTENSION),
    ("provider-retry.mjs", PROVIDER_RETRY),
    ("thinking-policy.mjs", THINKING_POLICY),
    ("service-tier.mjs", SERVICE_TIER),
    ("quota.mjs", QUOTA),
    ("history-repair.mjs", HISTORY_REPAIR),
    ("compaction-fallback.mjs", COMPACTION_FALLBACK),
    ("arsenal.mjs", ARSENAL),
    ("loop-guard.mjs", LOOP_GUARD),
    ("loop-guard.json", LOOP_POLICY),
    ("tool-owner.cjs", TOOL_OWNER),
];
/// The runtime's directory inside the app bundle's `Contents/Resources`.
pub const BUNDLED: &str = "agent-runtime";
/// A development or test binary (not inside an app bundle) may name a built runtime here; the app ignores it.
pub const DEV_OVERRIDE: &str = "HEIMDALL_AGENT_RUNTIME";
/// Pi's thinking level for every provider (Anthropic and OpenCode Go map it to effort `low`, the local engine to
/// `reasoning_effort` low). "think less and just answer right away and then think".
pub const THINKING: &str = "low";
pub const MODEL: &str = "qwen3.8-27b";
pub const PROVIDER: &str = "splash";
pub const BASE_URL: &str = "http://127.0.0.1:8100";

pub fn pi_version() -> String {
    serde_json::from_str::<Value>(PACKAGE).unwrap()["dependencies"]
        ["@earendil-works/pi-coding-agent"]
        .as_str()
        .unwrap()
        .into()
}
pub fn node_version() -> String {
    serde_json::from_str::<Value>(NODE).unwrap()["version"]
        .as_str()
        .unwrap()
        .into()
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn pin() -> String {
    hash(format!("{PACKAGE}{LOCK}{NODE}{EXTENSION}{TOOL_OWNER}{LOOP_GUARD}{LOOP_POLICY}{PROVIDER_RETRY}{THINKING_POLICY}{HISTORY_REPAIR}{SERVICE_TIER}{QUOTA}{COMPACTION_FALLBACK}{ARSENAL}").as_bytes())
}
/// Where `heimdall-agent-runtime build` keeps its verified runtime for development and test binaries.
pub fn dev_cache() -> PathBuf {
    checkout_root().join("target/agent-runtime")
}

/// The source checkout a development binary runs from: the nearest ancestor of the executable that holds this
/// workspace (Cargo.toml and crates/agent). Debug and test builds fall back to the checkout they were compiled in. A
/// release binary carries no build-machine path (VM finding P-03, 28 Oct): with no checkout around it, the current
/// folder, where the development-only files it names simply do not exist.
pub fn checkout_root() -> PathBuf {
    let found = std::env::current_exe().ok().and_then(|exe| {
        exe.ancestors().find(|a| a.join("Cargo.toml").is_file() && a.join("crates/agent").is_dir()).map(Path::to_path_buf)
    });
    if let Some(root) = found {
        return root;
    }
    #[cfg(debug_assertions)]
    return Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    #[cfg(not(debug_assertions))]
    std::env::current_dir().unwrap_or_default()
}
#[derive(Clone, Debug)]
pub struct Runtime {
    pub root: PathBuf,
    pub node: PathBuf,
    pub cli: PathBuf,
    pub extension: PathBuf,
}
impl Runtime {
    pub fn at(root: PathBuf) -> Self {
        Self {
            node: root.join("node/bin/node"),
            cli: root.join("node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"),
            extension: root.join("heimdall.ts"),
            root,
        }
    }
    /// The runtime this executable runs. Inside an app bundle: its own `Contents/Resources/agent-runtime`, nothing
    /// else. A development or test binary uses `HEIMDALL_AGENT_RUNTIME`, else the build cache's current runtime.
    pub fn installed() -> Result<Self> {
        let exe = std::env::current_exe().context("Heimdall executable")?;
        Ok(Self::at(locate(&exe, std::env::var_os(DEV_OVERRIDE).map(PathBuf::from))))
    }
    /// The installed runtime after its integrity check. Read-only: safe to call from any thread at any time.
    pub fn verified() -> Result<Self> {
        let runtime = Self::installed()?;
        runtime.verify()?;
        Ok(runtime)
    }
    fn verify_files(&self) -> Result<Value> {
        let receipt: Value = serde_json::from_slice(
            &fs::read(self.root.join("receipt.json")).context("runtime missing")?,
        )?;
        let files = receipt["files"]
            .as_object()
            .context("runtime integrity receipt")?;
        ensure!(
            serde_json::to_value(inventory(&self.root)?)? == Value::Object(files.clone()),
            "runtime files missing, changed or added"
        );
        Ok(receipt)
    }
    /// Every file matches the sealed inventory, the pin matches this build's sources, and Node and Pi report
    /// exactly the pinned versions.
    pub fn verify(&self) -> Result<()> {
        let receipt = self.verify_files()?;
        ensure!(receipt["pin"] == pin(), "runtime pin changed");
        let version = checked(
            Command::new(&self.node).arg("--version"),
            Duration::from_secs(10),
        )?;
        ensure!(
            version.trim() == format!("v{}", node_version()),
            "wrong Node version"
        );
        let version = checked(
            Command::new(&self.node)
                .arg(&self.cli)
                .arg("--version")
                .env("PI_OFFLINE", "1")
                .env("PI_TELEMETRY", "0"),
            Duration::from_secs(10),
        )?;
        ensure!(version.trim() == pi_version(), "wrong Pi version");
        Ok(())
    }
}
/// Whether `exe` runs from an app bundle's `Contents/MacOS` (the installed app), as opposed to a development or test
/// binary. Only the latter may read development overrides from the source checkout or the environment.
pub fn in_app_bundle(exe: &Path) -> bool {
    let macos = exe.parent();
    let contents = macos.and_then(Path::parent);
    macos.is_some_and(|m| m.ends_with("Contents/MacOS"))
        && contents.and_then(Path::parent).is_some_and(|app| app.extension().is_some_and(|e| e == "app"))
}
fn locate(exe: &Path, dev: Option<PathBuf>) -> PathBuf {
    let contents = exe.parent().and_then(Path::parent);
    match (in_app_bundle(exe), dev) {
        (true, _) => contents.unwrap().join("Resources").join(BUNDLED),
        (false, Some(dev)) => dev,
        (false, None) => dev_cache().join("current"),
    }
}
/// Build time only: record every file's hash and this build's pin. Run after signing, which changes Mach-O bytes.
pub fn seal(root: &Path) -> Result<()> {
    let receipt = json!({"pin": pin(), "files": inventory(root)?});
    atomic_write(&root.join("receipt.json"), &serde_json::to_vec(&receipt)?)
}
/// Earlier builds downloaded the runtime into `<data>/agent/runtime` and kept `runtime.previous`. The bundle
/// replaces both; remove them and their lock and staging leftovers. Returns the removed paths and bytes.
pub fn retire_downloaded(data: &Path) -> Result<(Vec<PathBuf>, u64)> {
    let agent = data.join("agent");
    let mut removed = vec![];
    let mut bytes = 0;
    let Ok(entries) = fs::read_dir(&agent) else { return Ok((removed, bytes)) };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !matches!(name.as_str(), "runtime" | "runtime.previous" | "runtime.lock") && !name.starts_with("runtime-stage-") {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            bytes += disk_bytes(&path);
            fs::remove_dir_all(&path).with_context(|| format!("remove {}", path.display()))?;
        } else {
            bytes += meta.len();
            fs::remove_file(&path)?;
        }
        removed.push(path);
    }
    Ok((removed, bytes))
}
fn disk_bytes(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries.flatten().map(|e| match fs::symlink_metadata(e.path()) {
        Ok(m) if m.is_dir() => disk_bytes(&e.path()),
        Ok(m) => m.len(),
        Err(_) => 0,
    }).sum()
}
fn inventory(root: &Path) -> Result<BTreeMap<String, String>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> Result<()> {
        for item in fs::read_dir(dir)? {
            let item = item?;
            let path = item.path();
            let key = path.strip_prefix(root)?.to_string_lossy().into_owned();
            if key == "receipt.json" {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                walk(root, &path, out)?;
            } else if metadata.file_type().is_symlink() {
                out.insert(key, format!("link:{}", fs::read_link(&path)?.display()));
            } else {
                let mut f = File::open(&path)?;
                let mut h = Sha256::new();
                let mut b = [0u8; 65536];
                loop {
                    let n = f.read(&mut b)?;
                    if n == 0 {
                        break;
                    }
                    h.update(&b[..n]);
                }
                out.insert(key, format!("{:x}", h.finalize()));
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out)?;
    Ok(out)
}
/// Small command output is drained concurrently: deadlines apply even to a noisy child.
pub fn checked(command: &mut Command, deadline: Duration) -> Result<String> {
    checked_with_start(command, deadline, Ok)
}
// The normal path keeps one absolute deadline from before spawn. Tests may
// start the execution/drain window after their descendants signal readiness,
// so cold interpreter launch cannot prevent the cleanup scenario from existing.
fn checked_with_start(
    command: &mut Command,
    deadline: Duration,
    ready: impl FnOnce(Instant) -> Result<Instant>,
) -> Result<String> {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    let began = Instant::now();
    // npm can detach script children too. Apply the same admission shim before
    // any Node preparation process executes, and retain its registry until drain.
    let tools=tempfile::tempdir()?;
    let preload=tools.path().join("tool-owner.cjs");fs::write(&preload,TOOL_OWNER)?;
    let groups=tools.path().join("groups");private_dir(&groups)?;
    let mut ownership=crate::process_owner::Owner::new();
    command.env_remove("HEIMDALL_PI_EXIT_FD").env("HEIMDALL_EPHEMERAL_OWNER","1").env(crate::process_owner::TOKEN,&ownership.token).env("HEIMDALL_TOOL_GROUPS",&groups).env("NODE_OPTIONS",format!("--require {}",serde_json::to_string(&preload.to_string_lossy())?));
    let helper=crate::process_owner::guardian()?;
    let mut wrapper=Command::new(helper);
    use std::os::fd::AsRawFd;
    let (_supervisor, lifetime) = crate::process_owner::supervisor_channel()?;
    let lifetime_fd = lifetime.as_raw_fd();
    wrapper.env("HEIMDALL_SUPERVISOR_FD", lifetime_fd.to_string());
    unsafe { wrapper.pre_exec(move || crate::process_owner::inherit_fd(lifetime_fd)); }
    wrapper.arg("--heimdall-guardian").arg(&groups).arg(command.get_program()).args(command.get_args()).process_group(0);
    if let Some(dir)=command.get_current_dir(){wrapper.current_dir(dir);}
    for (key,value) in command.get_envs(){match value{Some(value)=>{wrapper.env(key,value);},None=>{wrapper.env_remove(key);}}}
    let mut child = wrapper.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    drop(lifetime);
    ownership.root=crate::process_owner::Identity::read(child.id() as i32);
    if let Err(e)=crate::process_owner::admit(&groups,&mut ownership,child.id() as i32,false,Some(began+deadline)){let _=ownership.cleanup();let _=child.wait();return Err(e);}
    if let Err(e) = fs::write(groups.join("admitted"), b"") { let _=ownership.cleanup(); return Err(e.into()); }
    let began = match ready(began) {
        Ok(start) => start,
        Err(error) => { let _ = ownership.cleanup(); let _ = child.wait(); return Err(error); }
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let expired=std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let read = |mut input: Box<dyn Read + Send>,fd:i32| {
        let expired=expired.clone();
        std::thread::spawn(move || {
            let flags=unsafe{libc::fcntl(fd,libc::F_GETFL)};
            if flags<0 || unsafe{libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}<0 {return Err(std::io::Error::last_os_error());}
            let mut b = Vec::new();let mut chunk = [0u8; 8192];
            loop {
                if began.elapsed()>=deadline {expired.store(true,std::sync::atomic::Ordering::Release);break;}
                match input.read(&mut chunk) {
                    Ok(0)=>break,
                    Ok(n)=>if b.len()<65536 {b.extend_from_slice(&chunk[..n.min(65536-b.len())]);},
                    Err(e) if e.kind()==std::io::ErrorKind::WouldBlock=>{
                        let mut poll=libc::pollfd{fd,events:libc::POLLIN,revents:0};unsafe{libc::poll(&mut poll,1,10);}
                    },
                    Err(e) if e.kind()==std::io::ErrorKind::Interrupted=>{},
                    Err(e)=>return Err(e),
                }
            } Ok(b)
        })
    };
    let out_fd=stdout.as_raw_fd();let err_fd=stderr.as_raw_fd();
    let out = read(Box::new(stdout),out_fd);let err = read(Box::new(stderr),err_fd);
    // Keep the admitted birth identity even if the guardian died while draining.
    struct Owner { identity:crate::process_owner::Owner, _tools:tempfile::TempDir }
    impl Drop for Owner {fn drop(&mut self){let _=self.identity.cleanup();}}
    let owner=Owner{identity:ownership,_tools:tools};
    let status = loop {
        if let Ok(text)=fs::read_to_string(owner._tools.path().join("groups/exit.status")) {
            use std::os::unix::process::ExitStatusExt;
            break std::process::ExitStatus::from_raw(text.trim().parse()?);
        }
        if let Some(status) = child.try_wait()? { break status; }
        if began.elapsed() >= deadline {
            owner.identity.cleanup()?;
            let _ = child.wait();
            let _ = out.join();let _ = err.join();
            anyhow::bail!("runtime preparation timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    while !out.is_finished() || !err.is_finished() {
        if began.elapsed() >= deadline {
            owner.identity.cleanup()?;
            let _ = child.wait();
            let _ = out.join(); let _ = err.join();
            anyhow::bail!("runtime preparation output drain timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    owner.identity.cleanup()?;
    let _ = child.wait();
    let out = out.join().map_err(|_|anyhow::anyhow!("runtime output reader panicked"))??;
    let err = err.join().map_err(|_|anyhow::anyhow!("runtime error reader panicked"))??;
    ensure!(!expired.load(std::sync::atomic::Ordering::Acquire),"runtime preparation output drain timed out");
    ensure!(
        status.success(),
        "runtime preparation failed: {}",
        String::from_utf8_lossy(&err)
    );
    owner.identity.cleanup()?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}
pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
fn full_sync(file: &File) -> Result<()> {
    file.sync_all()?;
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file parent")?;
    private_dir(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    full_sync(file.as_file())?;
    file.persist(path)?;
    full_sync(&File::open(parent)?)?;
    Ok(())
}
/// Only verified engine metadata sizes the context. A stale guess never selects a model.
pub fn configure(data: &Path, context: u64) -> Result<PathBuf> {
    ensure!(
        (8192..=4_000_000).contains(&context),
        "invalid Splash context limit"
    );
    let limits = crate::provider::Limits::for_context(context, None);
    let config = json!({"providers":{PROVIDER:{"baseUrl":format!("{BASE_URL}/v1"),"api":"openai-completions","apiKey":"local-splash","models":[{"id":MODEL,"name":"Qwen3.8 27B","reasoning":true,"input":["text","image"],"contextWindow":context,"maxTokens":limits.response,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0},"thinkingLevelMap":{"off":"none"},"compat":{"supportsDeveloperRole":false,"supportsReasoningEffort":true,"maxTokensField":"max_tokens"}}]}}});
    write_config(data, &config, PROVIDER, MODEL, limits, 120_000)
}
/// How Pi authenticates to the API provider.
#[derive(Clone, Debug, PartialEq)]
pub enum PiAuth {
    /// A pasted key: `api_key` is Pi's `apiKey` (see `pi_api_key`); `workspace` is the workspace every request names
    /// (Anthropic keys not scoped to one), sent as the provider's workspace header.
    Key { workspace: Option<String>, api_key: String },
    /// A Console sign-in: `api_key` is Pi's per-request token command (installed app) or an environment
    /// reference (lab); the token also goes in `Authorization: Bearer`, with the Console's endpoint and headers.
    Console { api_key: String, base_url: Option<String>, headers: BTreeMap<String, String>, models: Vec<String> },
}
impl PiAuth {
    fn api_key(&self) -> &str {
        match self {
            Self::Key { api_key, .. } | Self::Console { api_key, .. } => api_key,
        }
    }
}

/// Pi's `apiKey` for a connected provider. The installed app's Pi asks this binary for the current key or token on
/// every request, so the key is in neither Pi's config nor its environment or arguments. A lab build names the
/// environment variable that `pi_credential_env` fills.
pub fn pi_api_key(p: &crate::provider::ProviderInfo, credential: &crate::provider::Credential, data: &Path, installed: bool) -> Result<String> {
    if installed {
        return crate::console::token_command(p, data);
    }
    Ok(match credential {
        crate::provider::Credential::Key(_) => format!("${}", p.key_env),
        crate::provider::Credential::Console(_) => format!("${}", crate::console::TOKEN_ENV),
    })
}

/// The environment a provider's Pi child receives for its credential. Only lab binaries hand a secret over here; the
/// installed app's Pi gets it from `pi_api_key`'s command, so the child's environment never holds it.
pub fn pi_credential_env(p: &crate::provider::ProviderInfo, credential: &crate::provider::Credential, installed: bool) -> Vec<(String, String)> {
    match (credential, installed) {
        (crate::provider::Credential::Key(key), false) => vec![(p.key_env.into(), key.clone())],
        (crate::provider::Credential::Console(session), false) => vec![(crate::console::TOKEN_ENV.into(), session.access.clone())],
        _ => vec![],
    }
}

/// A literal for Pi's config values, which run a leading `!` as a shell command and expand `$NAME`.
fn pi_literal(value: &str) -> String {
    let escaped = value.replace('$', "$$");
    match escaped.strip_prefix('!') { Some(rest) => format!("$!{rest}"), None => escaped }
}

/// The Console's endpoint for `model`, as OpenCode applies it: only for a model the config lists, and never a
/// format-specific base (".../openai/v1", ".../anthropic") for a model of the other format.
fn console_base(model: &crate::provider::ApiModel, base: &str, listed: &[String]) -> Option<String> {
    use crate::provider::Api;
    if !listed.iter().any(|m| *m == model.id) { return None; }
    let b = base.trim_end_matches('/');
    let root = b.strip_suffix("/v1").unwrap_or(b);
    let anthropic = model.api == Api::AnthropicMessages;
    if (anthropic && root.ends_with("/openai")) || (!anthropic && root.ends_with("/anthropic")) { return None; }
    // An AI SDK base (".../v1"); Pi's Anthropic client adds "/v1/messages" itself.
    Some(if anthropic { root.to_owned() } else { format!("{root}/v1") })
}

/// An added API model through Pi's built-in provider, which supplies the request formats and OpenCode's
/// session header. A model Pi's pinned `catalog` knows keeps Pi's definition with Heimdall's window; any
/// other gets a full definition on OpenCode's documented endpoint. No secret is written: `api_key` is a command or an
/// environment reference (see `pi_api_key`).
pub fn configure_api(data: &Path, model: &crate::provider::ApiModel, catalog: Option<&Value>, auth: &PiAuth) -> Result<PathBuf> {
    configure_api_carrying(data, model, catalog, auth, None).map(|(dir, _)| dir)
}
/// `configure_api` for a conversation whose last request had `history` tokens (`session::history_tokens`): a history
/// past the model's budget but inside its window is carried without an in-turn compaction (`Limits::carrying`).
/// Returns the limits Pi was given.
pub fn configure_api_carrying(data: &Path, model: &crate::provider::ApiModel, catalog: Option<&Value>, auth: &PiAuth, history: Option<u64>)
    -> Result<(PathBuf, crate::provider::Limits)> {
    if let Some(reason) = model.unsupported() {
        anyhow::bail!("{}: {reason}", model.name);
    }
    let provider = model.provider_info().context("provider not supported")?;
    let limits = model.limits().carrying(history);
    let known = catalog.and_then(|c| crate::provider::pi_entry(c, &model.id));
    let mut entry = json!({"apiKey": auth.api_key()});
    if let (PiAuth::Key { workspace: Some(id), .. }, Some(w)) = (auth, provider.workspaces) {
        // Every request Pi makes for this provider (replies, compaction summaries) names the workspace.
        entry["headers"] = json!({w.header: pi_literal(id)});
    }
    let mut base = None;
    if let PiAuth::Console { base_url, headers, models, .. } = auth {
        entry["authHeader"] = json!(true);
        if !headers.is_empty() {
            entry["headers"] = json!(headers.iter().map(|(k, v)| (k.clone(), pi_literal(v))).collect::<BTreeMap<_, _>>());
        }
        base = base_url.as_deref().and_then(|b| console_base(model, b, models));
    }
    match (known, base) {
        (Some(_), None) => {
            entry["modelOverrides"] = json!({&model.id: {"contextWindow": limits.window, "maxTokens": limits.response}});
            if model.thinking_off().is_some() {
                // Pi's catalog marks Off unsupported for the managed-effort Claude models; Heimdall offers it and the
                // extension sends Anthropic's form of it (runtime/thinking-policy.mjs).
                entry["modelOverrides"][&model.id]["thinkingLevelMap"] = json!({"off": "off"});
            }
        }
        (Some(known), Some(base)) => {
            let mut def = known.clone();
            if let Some(o) = def.as_object_mut() { o.remove("type"); o.remove("provider"); }
            def["baseUrl"] = json!(base);
            def["contextWindow"] = json!(limits.window);
            def["maxTokens"] = json!(limits.response);
            entry["models"] = json!([def]);
        }
        (None, base) => {
            let mut def = model.pi_definition_with(limits)?;
            if let Some(base) = base { def["baseUrl"] = json!(base); }
            entry["models"] = json!([def]);
        }
    }
    let config = json!({"providers": {provider.id: entry}});
    // SDK request timeouts include the fetch adapter's waits for a provider's reset (at most 15 minutes);
    // each network attempt still has its own deadline for the response to start.
    Ok((write_config(data, &config, provider.id, &model.id, limits, 30 * 60_000)?, limits))
}
fn write_config(data: &Path, models: &Value, provider: &str, model: &str, limits: crate::provider::Limits, timeout_ms: u64) -> Result<PathBuf> {
    let dir = data.join("agent/config");
    private_dir(&dir)?;
    atomic_write(&dir.join("models.json"), &serde_json::to_vec_pretty(models)?)?;
    let settings_path = dir.join("settings.json");
    let mut settings = if settings_path.is_file() {
        let v: Value = serde_json::from_slice(&fs::read(&settings_path)?)
            .context("Pi settings are invalid")?;
        ensure!(v.is_object(), "Pi settings must be an object");
        v
    } else {
        json!({"defaultProjectTrust":"never","cacheWarming":"off","retry":{"enabled":true,"maxRetries":2,"baseDelayMs":2000,"maxAgentDelayMs":10000},"httpIdleTimeoutMs":120000,"packages":[],"extensions":["-builtin:llama.cpp","-builtin:codemode","-builtin:tool-search"],"skills":[],"prompts":[]})
    };
    // Pi's MCP support is on: it reads the user's mcp.json (docs/connectors.md, route 3). Settings written before that
    // listed it off; drop only that entry so they match a fresh install.
    if let Some(list) = settings.get_mut("extensions").and_then(Value::as_array_mut) {
        list.retain(|e| e.as_str() != Some("-builtin:mcp"));
    }
    // The provider, thinking default, offline behavior and context reserves belong
    // to Heimdall. Other owned settings/extensions survive and are reread on reload.
    settings["retry"]["provider"]["timeoutMs"] = json!(timeout_ms);
    settings["defaultProvider"] = json!(provider);
    settings["defaultModel"] = json!(model);
    settings["defaultThinkingLevel"] = json!(THINKING);
    settings["defaultProjectTrust"] = json!("never");
    settings["enableInstallTelemetry"] = json!(false);
    settings["enableAnalytics"] = json!(false);
    settings["compaction"] =
        json!({"enabled":true,"reserveTokens":limits.pi_reserve(),"keepRecentTokens":limits.recent});
    atomic_write(&settings_path, &serde_json::to_vec_pretty(&settings)?)?;
    Ok(dir)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pins_are_exact_and_integrities_cover_every_npm_package() {
        assert_eq!(pi_version(), "1.4.0");
        let l: Value = serde_json::from_str(LOCK).unwrap();
        for (key, v) in l["packages"].as_object().unwrap() {
            if key.is_empty() {
                continue;
            }
            assert!(
                v["integrity"].as_str().unwrap().starts_with("sha512-"),
                "{key}"
            );
            assert!(!v["version"].as_str().unwrap().starts_with('^'));
        }
        assert_eq!(
            serde_json::from_str::<Value>(NODE).unwrap()["sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }
    #[test]
    fn models_and_compaction_use_real_context_and_no_bearer_is_saved() {
        let d = tempfile::tempdir().unwrap();
        let dir = configure(d.path(), 102400).unwrap();
        let model: Value =
            serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert_eq!(
            model["providers"][PROVIDER]["models"][0]["contextWindow"],
            102400
        );
        let settings: Value =
            serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["compaction"]["reserveTokens"], 12800, "Pi compacts when only the response still fits");
        assert_eq!(settings["defaultThinkingLevel"], THINKING);
        assert!(!dir.join("mcp.json").exists());
        assert!(configure(d.path(), 0).is_err());
    }
    /// Pi's MCP support is on by default, and settings that still list it off (written before docs/connectors.md route 3)
    /// lose only that entry at the next start; the other extension entries stay as they were.
    #[test]
    fn pi_mcp_support_is_on_and_old_settings_that_switched_it_off_are_migrated() {
        let d = tempfile::tempdir().unwrap();
        let dir = configure(d.path(), 102400).unwrap();
        let settings: Value = serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["extensions"], json!(["-builtin:llama.cpp", "-builtin:codemode", "-builtin:tool-search"]));
        fs::write(
            dir.join("settings.json"),
            serde_json::to_vec(&json!({"extensions": ["-builtin:mcp", "-builtin:llama.cpp", "-builtin:codemode", "-builtin:tool-search"]})).unwrap(),
        )
        .unwrap();
        configure(d.path(), 102400).unwrap();
        let settings: Value = serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["extensions"], json!(["-builtin:llama.cpp", "-builtin:codemode", "-builtin:tool-search"]));
        assert!(!dir.join("mcp.json").exists(), "Heimdall never writes the user's mcp.json");
    }
    #[test]
    fn api_config_uses_env_reference_haiku_limits_and_preserves_extensions() {
        use crate::provider::ApiModel;
        let d=tempfile::tempdir().unwrap();
        let dir=configure(d.path(),262144).unwrap();
        let mut settings:Value=serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        settings["ownedExtensionSetting"]=json!("keep");
        atomic_write(&dir.join("settings.json"),&serde_json::to_vec(&settings).unwrap()).unwrap();
        // Pi's pinned catalog knows Haiku: only the window and output limit are overridden.
        let catalog=json!({"anthropic-messages":{"chat:claude-haiku-5-5":{"id":"claude-haiku-5-5","api":"anthropic-messages","type":"chat"}}});
        configure_api(d.path(),&ApiModel::haiku(),Some(&catalog),&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_OPENCODE_API_KEY".into()}).unwrap();
        let raw=fs::read_to_string(dir.join("models.json")).unwrap();
        let model:Value=serde_json::from_str(&raw).unwrap();
        let go=&model["providers"]["opencode-go"];
        assert_eq!(go["apiKey"],"$HEIMDALL_OPENCODE_API_KEY");
        assert!(model["providers"]["splash"].is_null() && go["models"].is_null() && go["baseUrl"].is_null());
        // Pi gets Haiku's real 1M window (a long history's summary fits) and a reserve that compacts at 112,000 (Heimdall compacts first, at 85%: 108,800).
        assert_eq!(go["modelOverrides"]["claude-haiku-5-5"],json!({"contextWindow":1000000,"maxTokens":128000}),"F7: Haiku's own output limit");
        assert!(!raw.contains("sk-") && !raw.contains("Bearer"));
        let settings:Value=serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!((settings["defaultProvider"].as_str(),settings["defaultModel"].as_str()),(Some("opencode-go"),Some("claude-haiku-5-5")));
        assert_eq!(settings["compaction"]["reserveTokens"],ApiModel::haiku().limits().pi_reserve(),"the default budget's (the whole window)");
        assert_eq!(settings["compaction"]["keepRecentTokens"],20000);
        assert_eq!(settings["retry"]["provider"]["timeoutMs"],1800000);
        assert_eq!(settings["retry"]["maxRetries"],2);
        assert_eq!(settings["ownedExtensionSetting"],"keep");
        // Without Pi's entry the model gets a full definition on OpenCode's documented endpoint.
        configure_api(d.path(),&ApiModel::haiku(),None,&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_OPENCODE_API_KEY".into()}).unwrap();
        let model:Value=serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        let def=&model["providers"]["opencode-go"]["models"][0];
        assert_eq!((def["api"].as_str(),def["baseUrl"].as_str()),(Some("anthropic-messages"),Some("https://opencode.ai/inference/go/anthropic")));
        assert_eq!((def["contextWindow"].as_u64(),def["maxTokens"].as_u64()),(Some(1000000),Some(128000)));
        // Back to local: Splash only, 120 s request timeout.
        configure(d.path(),262144).unwrap();
        let model:Value=serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert!(model["providers"]["opencode-go"].is_null() && model["providers"][PROVIDER].is_object());
        let settings:Value=serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["retry"]["provider"]["timeoutMs"],120000);
        let mut google=ApiModel::haiku();google.api=crate::provider::Api::Google;
        assert!(configure_api(d.path(),&google,None,&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_OPENCODE_API_KEY".into()}).is_err());
        // A Console sign-in: a token command (no secret), Bearer header, the Console's endpoint and org header;
        // Pi's catalog entry keeps its compat flags with the Console base.
        let catalog=json!({"anthropic-messages":{"chat:claude-haiku-5-5":{"id":"claude-haiku-5-5","name":"Claude Haiku 5.5","api":"anthropic-messages","baseUrl":"https://opencode.ai/zen/go","compat":{"forceAdaptiveThinking":true},"type":"chat","provider":"opencode-go"}}});
        let auth=PiAuth::Console{api_key:"!'/Applications/H.app/heimdall' --provider-token opencode-go --data '/d/agent'".into(),base_url:Some("https://opencode.ai/zen/go/v1".into()),headers:BTreeMap::from([("x-org-id".into(),"org-a".into()),("x-note".into(),"!rm -rf ~ $HOME".into())]),models:vec!["claude-haiku-5-5".into()]};
        configure_api(d.path(),&ApiModel::haiku(),Some(&catalog),&auth).unwrap();
        let raw=fs::read_to_string(dir.join("models.json")).unwrap();
        let go:Value=serde_json::from_str::<Value>(&raw).unwrap()["providers"]["opencode-go"].clone();
        assert_eq!((go["authHeader"].as_bool(),go["headers"]["x-org-id"].as_str()),(Some(true),Some("org-a")));
        assert!(go["apiKey"].as_str().unwrap().starts_with("!'"));
        let def=&go["models"][0];
        assert_eq!((def["baseUrl"].as_str(),def["contextWindow"].as_u64(),def["maxTokens"].as_u64()),(Some("https://opencode.ai/zen/go"),Some(1000000),Some(128000)));
        assert_eq!(def["compat"]["forceAdaptiveThinking"],true);
        assert!(def.get("type").is_none() && def.get("provider").is_none());
        // Header values are literals for Pi: no command, no environment expansion.
        assert_eq!(go["headers"]["x-note"],"$!rm -rf ~ $$HOME");
        // An OpenAI-only base never reaches an Anthropic model, and an unlisted model keeps Pi's endpoint.
        let haiku=ApiModel::haiku();
        assert_eq!(console_base(&haiku,"https://opencode.ai/inference/go/openai/v1",&["claude-haiku-5-5".into()]),None);
        assert_eq!(console_base(&haiku,"https://opencode.ai/zen/go/v1",&[]),None);
        assert_eq!(console_base(&haiku,"https://opencode.ai/inference/go/anthropic/v1",&["claude-haiku-5-5".into()]).as_deref(),Some("https://opencode.ai/inference/go/anthropic"));
        let mut kimi=ApiModel::haiku();kimi.id="kimi-k3".into();kimi.api=crate::provider::Api::OpenaiCompletions;
        assert_eq!(console_base(&kimi,"https://opencode.ai/inference/go/anthropic",&["kimi-k3".into()]),None);
        assert_eq!(console_base(&kimi,"https://opencode.ai/inference/go/openai/v1/",&["kimi-k3".into()]).as_deref(),Some("https://opencode.ai/inference/go/openai/v1"));
        let unlisted=PiAuth::Console{api_key:"$X".into(),base_url:Some("https://opencode.ai/inference/go/openai/v1".into()),headers:BTreeMap::new(),models:vec![]};
        configure_api(d.path(),&haiku,Some(&catalog),&unlisted).unwrap();
        let model:Value=serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert!(model["providers"]["opencode-go"]["modelOverrides"]["claude-haiku-5-5"].is_object(),"Pi's own endpoint for an unlisted model");
    }
    #[test]
    fn provider_rate_limit_policy_waits_cancels_and_explains() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/provider-retry.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    #[test]
    fn thinking_off_requests_follow_anthropics_rules() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/thinking-policy.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    #[test]
    fn fast_requests_priority_only_on_openai_routes_and_logs_requested_and_reported_tiers() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/service-tier.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    #[test]
    fn plan_usage_limits_are_said_once_per_window_with_the_reset_openai_sent() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/quota.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    /// Setup ▸ Arsenal ▸ Use it: the extension registers Arsenal's own bridge from the setting, and nothing else.
    #[test]
    fn arsenal_s_bridge_is_registered_only_from_the_setting() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/arsenal.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    /// Bug note 2026-10-28T19-41-00: the extension's half of the compaction fallback (source, supply, last-resort cut).
    #[test]
    fn a_compaction_from_another_model_or_a_cut_is_written_by_pi_for_the_cut_it_was_made_for() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/compaction-fallback.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    #[test]
    fn a_refused_history_is_repaired_or_rebuilt_never_left_dead() {
        let output=Command::new("node").arg("--test").arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/history-repair.test.mjs")).output().unwrap();
        assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stdout));
    }
    #[test]
    fn anthropic_config_uses_pis_catalog_entry_its_env_key_and_the_slider_budget() {
        use crate::provider::ApiModel;
        let d=tempfile::tempdir().unwrap();
        let catalog=json!({"anthropic-messages":{"chat:claude-opus-5-5":{"id":"claude-opus-5-5","api":"anthropic-messages","type":"chat"}}});
        let mut opus=ApiModel::anthropic(crate::provider::OPUS).unwrap();
        let dir=configure_api(d.path(),&opus,Some(&catalog),&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_ANTHROPIC_API_KEY".into()}).unwrap();
        let raw=fs::read_to_string(dir.join("models.json")).unwrap();
        let model:Value=serde_json::from_str(&raw).unwrap();
        let a=&model["providers"]["anthropic"];
        assert_eq!(a["apiKey"],"$HEIMDALL_ANTHROPIC_API_KEY");
        assert!(a["baseUrl"].is_null() && a["models"].is_null(),"Pi's built-in provider and catalog entry: {a}");
        // F7: an answer may take Opus's own 128,000 output tokens (Pi trims it to the window per request).
        assert_eq!(a["modelOverrides"]["claude-opus-5-5"],json!({"contextWindow":1000000,"maxTokens":128000}));
        let settings:Value=serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!((settings["defaultProvider"].as_str(),settings["defaultModel"].as_str(),settings["defaultThinkingLevel"].as_str()),(Some("anthropic"),Some("claude-opus-5-5"),Some(THINKING)));
        assert_eq!(settings["compaction"]["reserveTokens"],ApiModel::anthropic(crate::provider::OPUS).unwrap().limits().pi_reserve(),"the default 500K budget's");
        // The slider at its 4th stop: compaction moves with it.
        opus.context_budget=Some(501_714);
        configure_api(d.path(),&opus,Some(&catalog),&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_ANTHROPIC_API_KEY".into()}).unwrap();
        let settings:Value=serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["compaction"]["reserveTokens"],1_000_000-501_714*90/100);
        // F6: a history carried from a bigger budget (600K at the 4th stop's 451,542 threshold) moves Pi's in-turn check
        // up by the budget's room; the idle mark stays.
        let (_,carried)=configure_api_carrying(d.path(),&opus,Some(&catalog),&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_ANTHROPIC_API_KEY".into()},Some(600_000)).unwrap();
        let settings:Value=serde_json::from_slice(&fs::read(dir.join("settings.json")).unwrap()).unwrap();
        assert_eq!((settings["compaction"]["reserveTokens"].as_u64(),carried.compact_at()),(Some(1_000_000-(600_000+501_714-501_714*90/100)),501_714*85/100));
        // A model Pi lacks gets a definition on api.anthropic.com.
        configure_api(d.path(),&opus,None,&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_ANTHROPIC_API_KEY".into()}).unwrap();
        let model:Value=serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert_eq!(model["providers"]["anthropic"]["models"][0]["baseUrl"],"https://api.anthropic.com");
        assert!(model["providers"]["anthropic"]["headers"].is_null(),"a scoped key names no workspace");
        // A key that is not scoped to a workspace: every request names it; the key itself stays an env reference.
        configure_api(d.path(),&opus,Some(&catalog),&PiAuth::Key{workspace:Some("wrkspc_01ABC".into()),api_key:"$HEIMDALL_ANTHROPIC_API_KEY".into()}).unwrap();
        let model:Value=serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert_eq!(model["providers"]["anthropic"]["headers"],json!({"anthropic-workspace-id":"wrkspc_01ABC"}));
        assert_eq!(model["providers"]["anthropic"]["apiKey"],"$HEIMDALL_ANTHROPIC_API_KEY");
        // Opus keeps Pi's thinking levels (it cannot think less than Low); Sonnet gets Off, which Pi's catalog marks
        // unsupported for its managed effort and the extension sends as Anthropic's `between_tools`.
        assert!(model["providers"]["anthropic"]["modelOverrides"]["claude-opus-5-5"]["thinkingLevelMap"].is_null());
        let catalog=json!({"anthropic-messages":{"chat:claude-sonnet-5-5":{"id":"claude-sonnet-5-5","api":"anthropic-messages","type":"chat"}}});
        configure_api(d.path(),&ApiModel::anthropic(crate::provider::SONNET).unwrap(),Some(&catalog),&PiAuth::Key{workspace:None,api_key:"$HEIMDALL_ANTHROPIC_API_KEY".into()}).unwrap();
        let model:Value=serde_json::from_slice(&fs::read(dir.join("models.json")).unwrap()).unwrap();
        assert_eq!(model["providers"]["anthropic"]["modelOverrides"]["claude-sonnet-5-5"]["thinkingLevelMap"],json!({"off":"off"}));
    }
    #[test]
    fn provider_secrets_stay_out_of_pis_config_and_environment_in_the_installed_app() {
        let _lab = crate::provider::LAB_ITEMS_TEST.blocking_lock();
        use crate::provider::{ApiModel, Credential, HAIKU, OPENCODE_GO, PROVIDERS};
        const KEY: &str = "sk-fake-config-0123456789abcdefghij";
        const TOKEN: &str = "fake-console-access-0123456789abcdefghij";
        let d = tempfile::tempdir().unwrap();
        for p in PROVIDERS {
            if std::env::var_os(p.key_env).is_some() || std::env::var_os(crate::console::TOKEN_ENV).is_some() { continue; }
            let model = if p.id == OPENCODE_GO { ApiModel::haiku() } else { ApiModel::anthropic(HAIKU).unwrap() };
            let session = crate::console::Session { access: TOKEN.into(), refresh: "fake-refresh".into(), expires_ms: u64::MAX / 2, org_id: None, server: "https://opencode.ai/console".into(), base_url: None, headers: BTreeMap::new(), models: vec![], client_id: None };
            for label in ["key", "sign-in"] {
                // One connection per provider: a pasted key, then a Console sign-in.
                if label == "key" {
                    crate::provider::save_key(p, KEY, None).unwrap();
                } else {
                    crate::provider::forget_key(p).unwrap();
                    crate::provider::save_console_session(p, &session).unwrap();
                }
                let credential = crate::provider::credential(p).unwrap();
                assert!(matches!((label, &credential), ("key", Credential::Key(_)) | ("sign-in", Credential::Console(_))), "{label}");
                // Installed app: Pi's apiKey is the key command, and the child environment carries nothing.
                let command = pi_api_key(p, &credential, d.path(), true).unwrap();
                assert!(command.starts_with("!'") && command.contains("--provider-token") && command.contains(p.id), "{command}");
                assert!(pi_credential_env(p, &credential, true).is_empty());
                let auth = match &credential {
                    Credential::Key(_) => PiAuth::Key { workspace: None, api_key: command.clone() },
                    Credential::Console(s) => PiAuth::Console { api_key: command.clone(), base_url: None, headers: s.request_headers(), models: vec![] },
                };
                let dir = configure_api(d.path(), &model, None, &auth).unwrap();
                let config = fs::read_to_string(dir.join("models.json")).unwrap();
                assert!(!config.contains(KEY) && !config.contains(TOKEN), "{label}: {config}");
                // Lab build: the environment reference Pi expands, and the harness fills that variable.
                let lab = pi_api_key(p, &credential, d.path(), false).unwrap();
                assert!(lab.starts_with('$') && !lab.contains(KEY) && !lab.contains(TOKEN), "{lab}");
                let env = pi_credential_env(p, &credential, false);
                assert_eq!(env.len(), 1);
                assert!(env[0].1 == KEY || env[0].1 == TOKEN);
                assert_eq!(lab.trim_start_matches('$'), env[0].0);
            }
            crate::provider::forget_console_session(p).unwrap();
            crate::provider::forget_key(p).unwrap();
        }
    }
    #[test]
    fn an_app_runs_only_its_own_bundled_runtime() {
        let app = Path::new("/Users/x/Applications/Heimdall.app/Contents/MacOS/Heimdall");
        // The development override never redirects an app bundle.
        assert_eq!(locate(app, Some("/elsewhere".into())), Path::new("/Users/x/Applications/Heimdall.app/Contents/Resources/agent-runtime"));
        let dev = Path::new("/repo/target/debug/heimdall");
        assert_eq!(locate(dev, Some("/lab/runtime".into())), Path::new("/lab/runtime"));
        assert_eq!(locate(dev, None), dev_cache().join("current"));
    }
    #[test]
    fn downloaded_runtime_copies_are_retired_and_owned_state_is_kept() {
        let d = tempfile::tempdir().unwrap();
        let agent = d.path().join("agent");
        for dir in ["runtime/node/bin", "runtime.previous/node_modules", "runtime-stage-abc", "config", "t3"] {
            fs::create_dir_all(agent.join(dir)).unwrap();
        }
        fs::write(agent.join("runtime/node/bin/node"), vec![0u8; 1000]).unwrap();
        fs::write(agent.join("runtime.previous/node_modules/x.js"), b"x").unwrap();
        fs::write(agent.join("runtime.lock"), b"").unwrap();
        fs::write(agent.join("config/settings.json"), b"{}").unwrap();
        let (removed, bytes) = retire_downloaded(d.path()).unwrap();
        assert_eq!(removed.len(), 4);
        assert_eq!(bytes, 1001);
        let left: Vec<_> = fs::read_dir(&agent).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!({ let mut l = left; l.sort(); l }, ["config", "t3"]);
        assert!(agent.join("config/settings.json").is_file());
        // Second launch: nothing left to do; a profile without an agent directory is fine.
        assert_eq!(retire_downloaded(d.path()).unwrap().0.len(), 0);
        assert_eq!(retire_downloaded(&d.path().join("absent")).unwrap().0.len(), 0);
    }
    #[test]
    fn corruption_and_missing_receipt_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let r = Runtime::at(d.path().into());
        assert!(r.verify().is_err());
        fs::write(d.path().join("a"), b"a").unwrap();
        let receipt = json!({"pin":pin(),"files":inventory(d.path()).unwrap()});
        atomic_write(
            &d.path().join("receipt.json"),
            &serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        fs::write(d.path().join("a"), b"corrupt").unwrap();
        assert!(r
            .verify()
            .unwrap_err()
            .to_string()
            .contains("runtime files"));
    }
    #[test]
    fn command_deadline_cuts_off_missing_or_hung_node() {
        assert!(checked(
            &mut Command::new("/nonexistent-node"),
            Duration::from_millis(50)
        )
        .is_err());
        let t = Instant::now();
        assert!(checked(
            Command::new("/bin/sleep").arg("5"),
            Duration::from_millis(50)
        )
        .is_err());
        assert!(t.elapsed() < Duration::from_secs(1));
    }
    /// Confirm the intended descendants exist before testing the one-second
    /// execution/drain deadline. The ten-second readiness bound diagnoses a
    /// broken fixture separately; it is not part of the cleanup timing claim.
    fn checked_fixture(command: &mut Command, pids: &[&Path]) -> anyhow::Error {
        let mut started = None;
        let error = checked_with_start(command, Duration::from_secs(1), |_| {
            let waiting = Instant::now();
            loop {
                let ready = pids.iter().all(|path| {
                    fs::read_to_string(path).ok()
                        .and_then(|text| text.trim().parse::<i32>().ok())
                        .and_then(crate::process_owner::Identity::read).is_some()
                });
                if ready { break; }
                anyhow::ensure!(waiting.elapsed() < Duration::from_secs(10), "fixture descendants did not become ready: {pids:?}");
                std::thread::sleep(Duration::from_millis(10));
            }
            let now = Instant::now();
            started = Some(now);
            Ok(now)
        }).unwrap_err();
        let elapsed = started.expect("cleanup scenario was never ready").elapsed();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(elapsed >= Duration::from_secs(1), "deadline fired early: {elapsed:?}");
        assert!(elapsed < Duration::from_secs(2), "deadline/cleanup took {elapsed:?}");
        error
    }

    fn assert_pid_reaped(pid: i32) {
        let began = Instant::now();
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(began.elapsed() < Duration::from_secs(1), "preparation child survived: {pid}");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn execution_and_inherited_pipe_drain_share_one_deadline() {
        let d = tempfile::tempdir().unwrap();
        let pid = d.path().join("pid");
        let error = checked_fixture(
            Command::new("/bin/sh").arg("-c").arg(format!("sleep 60 & echo $! > '{}'; exit 0", pid.display())),
            &[&pid],
        );
        assert!(error.to_string().contains("drain timed out"), "{error:#}");
        let pid: i32 = fs::read_to_string(pid).unwrap().trim().parse().unwrap();
        assert_pid_reaped(pid);
    }

    #[test]
    fn preparation_deadline_cleans_detached_node_descendants_and_inherited_pipes() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("pid");
        let script = d.path().join("detach.cjs");
        fs::write(&script, format!("const cp=require('node:child_process');const fs=require('node:fs');const c=cp.spawn('/bin/sleep',['60'],{{detached:true,stdio:['ignore',1,2]}});fs.writeFileSync({},String(c.pid));c.unref();", serde_json::to_string(&path.to_string_lossy()).unwrap())).unwrap();
        checked_fixture(Command::new("node").arg(script), &[&path]);
        let pid: i32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
        assert_pid_reaped(pid);
    }

    #[test]
    fn preparation_deadline_retires_setsid_and_double_fork_with_custom_environment() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("escape.py"), include_str!("../tests/fixtures/escaping-tool.py")).unwrap();
        let script = "const cp=require('node:child_process');cp.spawn('/bin/sh',['-c','/usr/bin/python3 escape.py'],{detached:true,stdio:['ignore',1,2],env:{PATH:'/usr/bin:/bin'}}).unref();";
        let paths = [d.path().join("setsid.pid"), d.path().join("double.pid")];
        checked_fixture(Command::new("node").args(["-e", script]).current_dir(d.path()), &[&paths[0], &paths[1]]);
        for path in paths {
            let pid: i32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
            assert!(crate::process_owner::Identity::read(pid).is_none(), "preparation escape survived: {pid}");
        }
    }

}
