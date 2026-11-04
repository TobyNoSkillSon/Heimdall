use crate::protocol::{Event, Framer};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{collections::VecDeque, path::PathBuf, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
    task::JoinHandle,
};
#[async_trait]
pub trait Harness: Send {
    fn max_text_bytes(&self) -> usize {
        9 * 1024 * 1024
    }
    fn serial_wakes(&self) -> bool {
        false
    }
    fn delivery_epoch(&self) -> Option<u64> {
        None
    }
    fn supports_steering(&self) -> bool { false }
    /// A temporary transport state that cannot safely accept any input.
    fn input_blocked(&self) -> Option<&'static str> { None }
    fn record_queue_fallback(&mut self, _: &Value, _: &str) -> Result<()> { Ok(()) }
    /// True only when a successful no-query send retains its content in the
    /// same durable model context. Ignoring shouldQuery is not append support.
    fn observation_append(&self) -> bool {
        false
    }
    async fn send(&mut self, wire: Value) -> Result<()>;
    /// Outer adapters must carry a permit captured before their own preparation.
    /// Unknown transports fail closed rather than silently acquire a new epoch.
    async fn send_with_permit(
        &mut self,
        _: Value,
        _: crate::delivery::DeliveryPermit,
    ) -> Result<()> {
        anyhow::bail!("harness cannot carry an external delivery permit")
    }
    async fn next(&mut self) -> Result<Option<Event>>;
    async fn shutdown(&mut self) -> Result<()>;
    /// A turn (or the transport's own maintenance) is running: notices and context appends wait for settlement.
    fn turn_active(&self) -> bool { false }
    /// Adds text to the same conversation without a turn (E1.10, E4.1). A transport that cannot keep context
    /// refuses, and what it carried stays queued.
    async fn append_context(&mut self, _: &str) -> Result<()> {
        Err(InputUnavailable("This agent transport cannot add context without a turn").into())
    }
    /// The next request's context tokens (unknown right after a compaction) and the window, when the transport
    /// can tell; a start compacts first when its catch-up would pass the threshold (E4.2).
    async fn context_tokens(&mut self) -> Result<Option<(Option<u64>, u64)>> { Ok(None) }
    async fn compact_history(&mut self) -> Result<()> { anyhow::bail!("this agent transport cannot compact") }
    /// The user's new words can restart the running turn instead of steering it: the turn has neither acted (no tool
    /// call, none being written) nor replied (no reply written or being written), so one answer can take his earlier
    /// and new words together (night pass H2: a steer waits for the request in flight and then gets an answer of its
    /// own, so a question said twice was answered twice). A cheap check on what the bridge has seen so far.
    fn can_rejoin(&self) -> bool { false }
    /// Ends the running turn without an answer, so the next input starts it again with everything the user said, if it
    /// still has neither acted nor replied once the events already waiting from the transport are taken in (night
    /// review 4: a tool call or reply that started streaming is never cut). Returns whether it ended the turn; when
    /// not, the user's words steer it. The ended turn's deliveries complete (their words stay in the conversation);
    /// nothing is resent.
    async fn rejoin(&mut self) -> Result<bool> { Ok(false) }
    /// When the provider's prompt cache for this conversation was last written (`maintenance::LastRequest`).
    fn last_request(&self) -> Option<crate::maintenance::LastRequest> { None }
    /// When the conversation's own record ends (its newest message, a failed request's included), Unix ms: a start's
    /// catch-up never repeats what the history holds (E4.1).
    fn history_end_ms(&self) -> Option<u64> { None }
}
/// Default is deliberately incapable of launching a process.
#[derive(Default)]
pub struct DryRun {
    pub log: Vec<Value>,
}
#[async_trait]
impl Harness for DryRun {
    fn observation_append(&self) -> bool {
        true
    }
    async fn send(&mut self, wire: Value) -> Result<()> {
        self.log
            .push(json!({"timestamp_ms":crate::now_ms(),"would_send":wire}));
        Ok(())
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        Ok(None)
    }
    async fn shutdown(&mut self) -> Result<()> {
        Ok(())
    }
    async fn append_context(&mut self, text: &str) -> Result<()> {
        self.log.push(json!({"timestamp_ms":crate::now_ms(),"would_append":text}));
        Ok(())
    }
}
#[derive(Debug)]
pub struct InputUnavailable(pub &'static str);
impl std::fmt::Display for InputUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.0) }
}
impl std::error::Error for InputUnavailable {}
pub type TransportRecorder = Arc<dyn Fn(&str, &Value) -> Result<()> + Send + Sync>;
struct Process {
    recorder: Arc<std::sync::Mutex<Option<TransportRecorder>>>,
    gate: Option<Arc<crate::delivery::DeliveryGate>>,
    generation: Option<u64>,
    child: Child,
    input: Option<ChildStdin>,
    events: mpsc::Receiver<Result<(Value, bool)>>,
    readers: Vec<JoinHandle<()>>,
    /// Output ended (EOF on stdout and stderr), seen before the OS may report the exit.
    closed: bool,
    alive: Arc<std::sync::atomic::AtomicBool>,
    /// Pi is compacting (`Pi::compaction_running`), or reported a compaction the bridge has not taken in yet
    /// (`compaction_reported`, set by the reader): Off waits for either before the kill (`watch_permit`).
    compacting: Arc<std::sync::atomic::AtomicBool>,
    compaction_reported: Arc<std::sync::atomic::AtomicBool>,
    stop_watch: Option<JoinHandle<()>>,
    owner: crate::process_owner::Owner,
    pi_identity: crate::process_owner::Identity,
    exit_signal: tokio::net::UnixStream,
    _supervisor: std::os::unix::net::UnixStream,
}
#[cfg(test)]
thread_local! { static TEST_BEFORE_GUARDIAN_RELEASE: std::cell::RefCell<Option<Box<dyn FnOnce(&Process)>>> = const { std::cell::RefCell::new(None) }; }
impl Process {
    #[cfg(test)]
    fn spawn_env(
        executable: &std::path::Path,
        args: &[String],
        cwd: &std::path::Path,
        env: &[(String, String)],
    ) -> Result<Self> {
        Self::spawn_env_permitted(executable, args, cwd, env, None)
    }
    fn spawn_env_permitted(
        executable: &std::path::Path, args: &[String], cwd: &std::path::Path,
        env: &[(String,String)], permit: Option<crate::delivery::DeliveryPermit>,
    ) -> Result<Self> {
        if let Some(p) = &permit { p.check()?; }
        let dir=PathBuf::from(env.iter().find(|(k,_)|k=="HEIMDALL_TOOL_GROUPS").context("ownership directory missing")?.1.clone());
        let token=env.iter().find(|(k,_)|k==crate::process_owner::TOKEN).context("process owner token missing")?.1.clone();
        let helper=crate::process_owner::guardian()?;
        use std::os::fd::AsRawFd;
        let (exit_reader,exit_writer)=std::os::unix::net::UnixStream::pair()?;
        let exit_fd=exit_writer.as_raw_fd();
        let (supervisor, lifetime) = crate::process_owner::supervisor_channel()?;
        let lifetime_fd = lifetime.as_raw_fd();
        let mut command = Command::new(helper);
        unsafe {command.pre_exec(move|| {
            crate::process_owner::inherit_fd(lifetime_fd)?;
            let flags=libc::fcntl(exit_fd,libc::F_GETFD);
            if flags<0 || libc::fcntl(exit_fd,libc::F_SETFD,flags & !libc::FD_CLOEXEC)<0 {return Err(std::io::Error::last_os_error());}
            Ok(())
        });}
        command.arg("--heimdall-guardian").arg(&dir).arg(executable);
        command.env_clear();
        command.env("HEIMDALL_PI_EXIT_FD",exit_fd.to_string());
        command.env("HEIMDALL_SUPERVISOR_FD",lifetime_fd.to_string());
        // Heimdall's folders and edition (docs/home.md), so the CLIs Pi runs use the app's own profile (and, in the
        // public edition, its own islands folder).
        for key in [
            "HOME", "PATH", "TMPDIR", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "HEIMDALL_DATA", "HEIMDALL_HOME",
            "HEIMDALL_BUNDLE_ID", "FLOATING_ISLANDS_DIR", "HEIMDALL_FOLDERS_EXPORTED", "HEIMDALL_ISLANDS_EXPORTED",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        // The runtime's own words name the owner only in the personal edition (VM finding P-04, 28 Oct).
        command.env("HEIMDALL_EDITION", if cfg!(feature = "public") { "public" } else { "personal" });
        for (key, value) in env {
            command.env(key, value);
        }
        command.process_group(0);
        // Descriptors the child inherits (its lock, and the MCP token pipe it reads and closes).
        for name in ["HEIMDALL_LOCK_FD", "HEIMDALL_MCP_TOKEN_FD"] {
            if let Some((_, fd)) = env.iter().find(|(key, _)| key == name) {
                let fd = fd.parse::<i32>()?;
                // Change CLOEXEC only in THIS forked child. Clearing it in the
                // parent races other concurrent spawns and leaks their session locks.
                unsafe {
                    command.pre_exec(move || {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    });
                }
            }
        }
        let mut child = command
            .args(args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("launch agent harness")?;
        drop(exit_writer);
        drop(lifetime);
        let mut owner = crate::process_owner::Owner {
            token,
            root: crate::process_owner::Identity::read(child.id().unwrap() as i32),
            ..crate::process_owner::Owner::new()
        };
        let admitted=(|| {
            exit_reader.set_nonblocking(true)?;
            let exit_signal=tokio::net::UnixStream::from_std(exit_reader)?;
            let pi_identity=crate::process_owner::admit(&dir,&mut owner,child.id().unwrap() as i32,true,None)?;
            Ok::<_,anyhow::Error>((pi_identity,exit_signal))
        })();
        let (pi_identity,exit_signal)=match admitted {
            Ok(pair)=>pair,Err(e)=>{let _=owner.cleanup();return Err(e);}
        };
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, events) = mpsc::channel(128);
        let recorder = Arc::new(std::sync::Mutex::new(None::<TransportRecorder>));
        let stdout_recorder = recorder.clone();
        let stderr_recorder = recorder.clone();
        let errors = tx.clone();
        // Pi's compaction as it reports it, the moment it is read: Off's stop watcher must not wait for the bridge to
        // take the event in (it may be busy with a command) before it knows a compaction runs.
        let reported = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let compaction_reported = reported.clone();
        let read = tokio::spawn(async move {
            let mut stdout = stdout;
            let mut chunk = [0u8; 8192];
            let mut framer = Framer::default();
            loop {
                match stdout.read(&mut chunk).await {
                    Ok(0) => {
                        if let Err(e) = framer.finish() {
                            let _ = tx.send(Err(e)).await;
                        }
                        break;
                    }
                    Ok(n) => match framer.push(&chunk[..n]) {
                        Ok(frames) => {
                            for v in frames {
                                match v["type"].as_str() {
                                    Some("compaction_start") => reported.store(true, std::sync::atomic::Ordering::Release),
                                    Some("compaction_end") => reported.store(false, std::sync::atomic::Ordering::Release),
                                    Some("response") if v["command"] == "compact" => reported.store(false, std::sync::atomic::Ordering::Release),
                                    _ => {}
                                }
                                let hook = stdout_recorder.lock().unwrap().clone();
                                let recorded = hook.is_some();
                                if let Some(hook) = hook {
                                    if let Err(e) = hook("reply", &v) {
                                        let _ = tx.send(Err(e)).await;
                                        return;
                                    }
                                }
                                if tx.send(Ok((v, recorded))).await.is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            break;
                        }
                    },
                    Err(e) => {
                        let _ = tx.send(Err(e.into())).await;
                        break;
                    }
                }
            }
        });
        let drain = tokio::spawn(async move {
            let mut stderr = stderr;
            let mut chunk = [0u8; 8192];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let value =
                    json!({"type":"heimdall_stderr","text":String::from_utf8_lossy(&chunk[..n])});
                let hook = stderr_recorder.lock().unwrap().clone();
                let recorded = hook.is_some();
                if let Some(hook) = hook {
                    if let Err(e) = hook("reply", &value) {
                        let _ = errors.send(Err(e)).await;
                        break;
                    }
                }
                if errors.send(Ok((value, recorded))).await.is_err() {
                    break;
                }
            }
        });
        let mut process = Self {
            recorder,
            gate: None,
            generation: None,
            child,
            input,
            events,
            readers: vec![read, drain],
            closed: false,
            alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            compacting: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            compaction_reported,
            stop_watch: None,
            owner,
            pi_identity,
            exit_signal,
            _supervisor: supervisor,
        };
        // Install the original epoch watcher while the child is still blocked
        // behind admission. A launch can never borrow a later On generation.
        if let Some(p) = &permit { process.watch_permit(p.clone()); }
        #[cfg(test)]
        TEST_BEFORE_GUARDIAN_RELEASE.with(|hook| { if let Some(hook)=hook.borrow_mut().take() { hook(&process); } });
        let release = || { std::fs::write(dir.join("admitted"), b"")?; Ok(()) };
        match &permit { Some(p) => p.with(release), None => release() }?;
        Ok(process)
    }
    fn watch_permit(&mut self, permit: crate::delivery::DeliveryPermit) {
        if let Some(old) = self.stop_watch.take() { old.abort(); }
        let alive = self.alive.clone();
        let (compacting, reported) = (self.compacting.clone(), self.compaction_reported.clone());
        let owner = self.owner.clone();
        let pi_identity = self.pi_identity;
        self.gate = Some(permit.gate());
        self.generation = Some(permit.generation());
        self.stop_watch = Some(tokio::spawn(async move {
            permit.cancelled().await;
            // Crucial work finishes first, bounded: a running compaction (22 Oct 16:02: "compaction
            // must never be interrupted") and a state-writing tool call. No tool of Pi's own runs during a compaction.
            let (gate, off) = (permit.gate(), tokio::time::Instant::now());
            let compaction = || compacting.load(std::sync::atomic::Ordering::Acquire) || reported.load(std::sync::atomic::Ordering::Acquire);
            while (compaction() && off.elapsed() < crate::delivery::COMPACTION_WAIT)
                || (gate.writes_in_flight() > 0 && off.elapsed() < crate::delivery::WRITE_WAIT) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            if alive.load(std::sync::atomic::Ordering::Acquire) {
                pi_identity.kill();
                let _ = tokio::task::spawn_blocking(move || owner.cleanup()).await;
            }
        }));
    }
    /// The child can no longer take a message: it exited, its output ended, or its
    /// stdin was dropped after a cancelled partial write (which also killed it).
    fn gone(&mut self) -> Result<bool> {
        let gone = self.closed || self.input.is_none() || crate::process_owner::Identity::read(self.pi_identity.pid)!=Some(self.pi_identity) || self.child.try_wait()?.is_some();
        if gone {
            self.kill_owned();
            self.alive
                .store(false, std::sync::atomic::Ordering::Release);
        }
        Ok(gone)
    }
    fn recorder(&self) -> Option<TransportRecorder> {
        self.recorder.lock().unwrap().clone()
    }
    fn set_recorder(&self, recorder: Option<TransportRecorder>) {
        *self.recorder.lock().unwrap() = recorder;
    }
    fn permit(&self) -> Result<Option<crate::delivery::DeliveryPermit>> {
        self.gate.as_ref().map(|gate| {
            let p=gate.permit()?;
            if Some(p.generation())!=self.generation { return Err(crate::delivery::DeliveryBlocked{written:0,total:0}.into()); }
            Ok(p)
        }).transpose()
    }
    async fn send_permitted(
        &mut self,
        v: Value,
        permit: Option<crate::delivery::DeliveryPermit>,
    ) -> Result<()> {
        if let Some(record) = self.recorder() {
            record("send", &v)?;
        }
        let input = self.input.as_mut().context("harness stdin closed")?;
        let mut b = serde_json::to_vec(&v)?;
        b.push(b'\n');
        let result = if let Some(permit) = &permit {
            permit.write(input, &b).await
        } else {
            async {
                input.write_all(&b).await?;
                input.flush().await?;
                Ok(())
            }
            .await
        };
        if let Err(e) = &result {
            if let Some(off) = e.downcast_ref::<crate::delivery::DeliveryBlocked>() {
                if let Some(record) = self.recorder() {
                    record(
                        "cancelled",
                        &json!({"written_bytes":off.written,"total_bytes":off.total,"uncertain":off.written>0}),
                    )?;
                }
                // A cancelled partial JSON envelope must never be completed on reuse.
                self.input.take();
                self.kill_owned();
            }
        }
        result?;
        if let Some(record) = self.recorder() {
            record("sent", &v)?;
        }
        Ok(())
    }
    /// An event Pi already wrote and the reader already took in, without waiting; `None` when nothing is waiting.
    fn try_next(&mut self) -> Result<Option<Value>> {
        let Ok(value) = self.events.try_recv() else { return Ok(None) };
        let (value, recorded) = value?;
        if !recorded {
            if let Some(record) = self.recorder() {
                record("reply", &value)?;
            }
        }
        Ok(Some(value))
    }
    async fn next(&mut self) -> Result<Option<Value>> {
        let mut exited=[0u8;1];
        let value = tokio::select! {
            biased;
            v = self.events.recv() => v.transpose()?,
            status = self.child.wait() => { status?; self.kill_owned(); None },
            exit = self.exit_signal.read(&mut exited) => { exit?; self.kill_owned(); None },
        };
        if let Some((value, recorded)) = value {
            if !recorded {
                if let Some(record) = self.recorder() {
                    record("reply", &value)?;
                }
            }
            Ok(Some(value))
        } else {
            self.kill_owned();
            self.closed = true;
            Ok(None)
        }
    }
    async fn shutdown(&mut self) -> Result<()> {
        self.alive
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(watch) = self.stop_watch.take() {
            watch.abort();let _=watch.await;
        }
        self.input.take();
        self.kill_owned();
        if tokio::time::timeout(std::time::Duration::from_secs(3), self.child.wait())
            .await
            .is_err()
        {
            self.kill_owned();
            self.child.wait().await?;
        }
        self.cleanup_tools()?;
        // Drain producer queues after EOF before aborting readers: replies remain
        // evidence even when the bridge cancelled and no longer calls next().
        let drained = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while let Some(value) = self.events.recv().await {
                let (value, recorded) = value?;
                if !recorded {
                    if let Some(record) = self.recorder() {
                        record("reply", &value)?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await;
        for r in self.readers.drain(..) {
            r.abort();
        }
        drained.context("harness output drain timed out")??;
        Ok(())
    }
}
impl Process {
    fn kill_owned(&mut self) {
        self.pi_identity.kill();
        let _ = self.owner.cleanup();
    }
    fn cleanup_tools(&mut self) -> Result<()> {
        self.owner.cleanup()?;
        self.owner.root = None;
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        self.alive
            .store(false, std::sync::atomic::Ordering::Release);
        if let Some(watch) = self.stop_watch.take() {
            watch.abort();
        }
        self.kill_owned();
        for r in &self.readers {
            r.abort();
        }
    }
}
fn last_entry_id(path: &std::path::Path) -> Result<Option<String>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    fn leaf(parts:&mut VecDeque<Vec<u8>>) -> Option<String> {
        let mut line=Vec::with_capacity(parts.iter().map(Vec::len).sum());
        for part in parts.drain(..){line.extend_from_slice(&part);}
        serde_json::from_slice::<Value>(&line).ok()?.get("id")?.as_str().map(str::to_owned)
    }
    // Keep only the current record while reading backwards in fixed blocks.
    // Each oversized record is assembled and parsed once, independent of size.
    let mut end=file.metadata()?.len();let mut parts=VecDeque::new();
    while end>0 {
        let start=end.saturating_sub(64*1024);file.seek(SeekFrom::Start(start))?;
        let mut block=vec![0; (end-start) as usize];file.read_exact(&mut block)?;
        let mut right=block.len();
        for i in (0..block.len()).rev().filter(|&i|block[i]==b'\n') {
            parts.push_front(block[i+1..right].to_vec());
            if let Some(id)=leaf(&mut parts){return Ok(Some(id));}
            right=i;
        }
        parts.push_front(block[..right].to_vec());end=start;
    }
    Ok(leaf(&mut parts))
}

pub(crate) fn kill_tools(root: &std::path::Path) -> Result<()> {
    crate::process_owner::cleanup_registry(root)
}
#[derive(Debug)]
pub struct EngineUnavailable;
impl std::fmt::Display for EngineUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Agent waiting for the engine")
    }
}
impl std::error::Error for EngineUnavailable {}
/// The selected API model cannot start: the reason is what the menu and the agent window show.
#[derive(Debug)]
pub struct ProviderUnavailable(pub String);
impl std::fmt::Display for ProviderUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ProviderUnavailable {}

/// The history size above which compacting beats re-sending it to a cold one-hour cache (`maintenance`). A cold
/// cache re-sends the whole history as a new one-hour write: 2× input on all P tokens. Compacting first costs P at
/// 1× (the summary request is not cached), the summary's output (≈10K input tokens' worth) and a 2× write of what
/// remains: system and tools (≈18K), the kept tail (`recent`) and the summary (≈3K):
/// P = 2 × (18K system and tools + kept tail + 3K summary) + 10K summary output. 92,000 tokens with the 20K tail.
pub fn cold_cache_break_even(limits: crate::provider::Limits) -> u64 {
    2 * (18_000 + limits.recent + 3_000) + 10_000
}

/// The extension command that appends context without a turn (runtime/heimdall.ts).
pub const CONTEXT_COMMAND: &str = "heimdall_context";
/// The extension command that records the reload outcome note, once the replacement start is verified.
pub const RELOAD_NOTE_COMMAND: &str = "heimdall_reload_note";
/// The extension commands of the compaction fallback (runtime/compaction-fallback.mjs; bug note 2026-10-28T19-41-00):
/// what the last compaction would summarise, written to a file, and the next compaction's result from Heimdall.
pub const COMPACTION_SOURCE_COMMAND: &str = "heimdall_compaction_source";
pub const COMPACTION_SUPPLY_COMMAND: &str = "heimdall_compaction_supply";

#[derive(Clone)]
pub struct PiConfig {
    /// The added API model the agent runs; `None` runs the local engine's model.
    pub api_model: Option<crate::provider::ApiModel>,
    pub executable: PathBuf,
    pub cli: Option<PathBuf>,
    pub session: PathBuf,
    pub working_dir: PathBuf,
    pub agent_dir: PathBuf,
    pub prompt: Option<PathBuf>,
    pub extension: Option<PathBuf>,
    pub mcp: Option<(String, String)>,
    pub engine: Option<crate::supervision::EngineProbe>,
    pub engine_signal: Option<Arc<std::sync::atomic::AtomicU8>>,
    pub inactivity: std::time::Duration,
    /// How long a running compaction may stay silent. Pi writes nothing while the model summarises, and a large
    /// session on a slow model takes minutes: the plain `inactivity` killed every Sol compaction at 230k tokens
    /// (27 Oct 03:24-03:46, six attempts, then "could not recover").
    pub compaction_inactivity: std::time::Duration,
    pub max_text_bytes: usize,
    /// Pi's thinking level at launch (`--thinking`); the menu's later changes go to the running Pi
    /// (`set_thinking_level`).
    pub thinking: String,
    /// Where every tool call and its outcome is logged (`crate::tool_log`); the prompt names this path.
    pub tool_log: Option<PathBuf>,
}
impl PiConfig {
    pub fn owned(
        runtime: &crate::runtime::Runtime,
        data: &std::path::Path,
        agent_dir: PathBuf,
        url: String,
        token: String,
    ) -> Self {
        Self {
            api_model: None,
            executable: runtime.node.clone(),
            cli: Some(runtime.cli.clone()),
            session: data.join("pi-session.jsonl"),
            working_dir: working_dir(),
            agent_dir,
            prompt: Some(crate::runtime::checkout_root().join("crates/agent").join(PROMPT_FILE)),
            extension: Some(runtime.extension.clone()),
            mcp: Some((url, token)),
            engine_signal: None,
            engine: crate::supervision::EngineProbe::new(crate::runtime::BASE_URL).ok(),
            inactivity: std::time::Duration::from_secs(180),
            compaction_inactivity: crate::delivery::COMPACTION_WAIT,
            max_text_bytes: 9 * 1024 * 1024,
            thinking: crate::runtime::THINKING.into(),
            tool_log: Some(crate::tool_log::path(data)),
        }
    }
    pub fn fixture(executable: PathBuf, session: PathBuf, cwd: PathBuf) -> Self {
        Self {
            api_model: None,
            agent_dir: session.parent().unwrap().join("fixture-config"),
            executable,
            cli: None,
            session,
            working_dir: cwd,
            prompt: None,
            extension: None,
            mcp: None,
            engine: None,
            engine_signal: None,
            inactivity: std::time::Duration::from_secs(180),
            compaction_inactivity: crate::delivery::COMPACTION_WAIT,
            max_text_bytes: 9 * 1024 * 1024,
            thinking: crate::runtime::THINKING.into(),
            tool_log: None,
        }
    }
    /// The (provider, model id) this start asks Pi for: the selected API model, else the local engine's.
    pub fn selected_model(&self) -> (&str, &str) {
        match &self.api_model {
            Some(m) => (m.provider.as_str(), m.id.as_str()),
            None => (crate::runtime::PROVIDER, crate::runtime::MODEL),
        }
    }
    fn spawn(&self, lock_fd: i32, permit: Option<crate::delivery::DeliveryPermit>) -> Result<Process> {
        self.spawn_with(lock_fd, permit, crate::provider::is_installed_app())
    }
    /// `installed` selects the installed app's credential handoff (no secret in the child's environment) or a lab
    /// build's environment handoff.
    fn spawn_with(&self, lock_fd: i32, permit: Option<crate::delivery::DeliveryPermit>, installed: bool) -> Result<Process> {
        let registry = self.session.parent().unwrap().join("tool-groups");
        crate::runtime::private_dir(&registry)?;
        kill_tools(&registry)?;
        // Old watchers/destructors retain only their instance's directory. They
        // cannot touch tools admitted by the replacement on this session.
        let groups=registry.join(uuid::Uuid::new_v4().to_string());
        crate::runtime::private_dir(&groups)?;
        let owner=crate::process_owner::Owner::new();
        owner.persist(&groups)?;
        let mut args = vec![];
        if self.cli.is_some() {
            let preload = self.cli.as_ref().unwrap().ancestors().nth(6).context("runtime root")?.join("tool-owner.cjs");
            args.extend(["--require".into(), preload.to_string_lossy().into_owned()]);
        }
        if let Some(cli) = &self.cli {
            args.push(cli.to_string_lossy().into_owned());
        }
        args.extend([
            "--mode".into(),
            "rpc".into(),
            "--session".into(),
            self.session.to_string_lossy().into_owned(),
            "--session-dir".into(),
            self.session
                .parent()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--no-approve".into(),
            "--offline".into(),
            // Only Heimdall's own skill folder (`crate::skills`), never skills installed for other agents.
            "--no-skills".into(),
            "--no-prompt-templates".into(),
            // Heimdall has its own system prompt; the Vault's AGENTS.md/CLAUDE.md are for T3 workers
            // ("Heimdall should have his own system prompt … Vault is something separate").
            "--no-context-files".into(),
        ]);
        if self.cli.is_some() {
            let (provider, model) = self.selected_model();
            args.extend([
                "--provider".into(),
                provider.into(),
                "--model".into(),
                model.into(),
                "--thinking".into(),
                self.thinking.clone(),
            ]);
        }
        if let Some(ext) = &self.extension {
            args.extend(["-e".into(), ext.to_string_lossy().into_owned()]);
        }
        if let Some(prompt) = &self.prompt {
            let exe = std::env::current_exe()?;
            // The procedures the minimal prompt leaves out (docs/skills.md): each bundled skill is copied in once; a
            // reload rereads the folder, so an edited skill is live after `reload_me`. A failed copy never stops Pi.
            let skills = crate::skills::folder(&self.agent_dir);
            if let Err(e) = crate::skills::seed_and_upgrade(&crate::skills::bundled(&exe), &skills) {
                eprintln!("Bundled skills not copied: {e:#}");
            }
            args.extend(["--skill".into(), skills.to_string_lossy().into_owned()]);
            let text = fill_prompt(&prompt_text(prompt, &exe)?, &exe, self.tool_log.as_deref(), self.session.parent());
            // A file rather than argv carries the prompt (no process-list disclosure).
            let target = self.agent_dir.join("APPEND_SYSTEM.md");
            crate::runtime::atomic_write(&target, text.as_bytes())?;
        }
        let mut env = vec![(crate::process_owner::TOKEN.into(),owner.token),("HEIMDALL_TOOL_GROUPS".into(), groups.to_string_lossy().into_owned()),
            (
                "PI_CODING_AGENT_DIR".into(),
                self.agent_dir.to_string_lossy().into_owned(),
            ),
            ("PI_OFFLINE".into(), "1".into()),
            ("PI_SKIP_VERSION_CHECK".into(), "1".into()),
            ("PI_TELEMETRY".into(), "0".into()),
            ("HEIMDALL_LOCK_FD".into(), lock_fd.to_string()),
        ];
        if let Some(model) = &self.api_model {
            let provider = model.provider_info().context("provider not supported")?;
            env.push(("HEIMDALL_API_PROVIDER".into(), provider.id.into()));
            if let Some(off) = model.thinking_off() {
                env.push(("HEIMDALL_THINKING_OFF".into(), off.into()));
            }
            if provider.hour_cache {
                // Pi marks the stable prefix (system, tools, history) with Anthropic's one-hour cache.
                env.push(("PI_CACHE_RETENTION".into(), "long".into()));
                // A cold cache is compacted while idle, never in front of a wake (`maintenance`, night pass H1).
            }
            // The installed app's Pi runs its key command per request; only a lab build hands a secret over here.
            env.extend(crate::runtime::pi_credential_env(provider, &crate::provider::credential(provider)?, installed));
        }
        // Speed (runtime/service-tier.mjs): the extension reads agent_speed from settings.json per request and logs the
        // requested and reported tiers beside the tool log. The data folder is the tool log's grandparent.
        if let Some(data) = self.tool_log.as_deref().and_then(std::path::Path::parent).and_then(std::path::Path::parent) {
            env.push(("HEIMDALL_SETTINGS_FILE".into(), data.join("settings.json").display().to_string()));
            env.push(("HEIMDALL_TIER_LOG".into(), data.join("logs").join("provider-tier.jsonl").display().to_string()));
        }
        // The MCP token travels in a pipe that Pi's extension reads and closes; it is never in the environment or argv.
        let mut token_pipe = None;
        if let Some((url, token)) = &self.mcp {
            use std::io::Write;
            use std::os::fd::AsRawFd;
            let (reader, mut writer) = std::io::pipe()?;
            writer.write_all(token.as_bytes())?;
            drop(writer);
            env.extend([
                ("HEIMDALL_MCP_URL".into(), url.clone()),
                ("HEIMDALL_MCP_TOKEN_FD".into(), reader.as_raw_fd().to_string()),
            ]);
            token_pipe = Some(reader);
        }
        let process = Process::spawn_env_permitted(&self.executable, &args, &self.working_dir, &env, permit);
        drop(token_pipe);
        process
    }
}
/// The system prompt Pi gets. The installed app always uses the prompt compiled into this build, never a file from
/// the source checkout (whose branch can differ from the build). A development or test binary may use `path`
/// (the checkout's prompt or a lab prompt) when it exists.
fn prompt_text(path: &std::path::Path, exe: &std::path::Path) -> Result<String> {
    if !crate::runtime::in_app_bundle(exe) && path.is_file() {
        return Ok(std::fs::read_to_string(path)?);
    }
    Ok(PROMPT.into())
}
/// Every runtime fill Heimdall's prompt gets, for Pi's launch and the relay (`relay_prompt::text`): the configured names
/// (Personalise; unchanged for Heimdall and the user), the tool log, the connectors guide, the emergency protocol, the docs
/// folder, and the taste ledger's path and commands for the profile in `data` (`crate::taste`).
pub fn fill_prompt(text: &str, exe: &std::path::Path, tool_log: Option<&std::path::Path>, data: Option<&std::path::Path>) -> String {
    let text = crate::identity::fill(text, &crate::identity::current());
    let text = with_tool_log(&text, tool_log);
    let text = with_connectors_doc(&text, &connectors_doc(exe));
    let text = text.replace(EMERGENCY_PLACEHOLDER, &emergency_doc(exe).display().to_string());
    let text = text.replace(DOCS_PLACEHOLDER, &docs_dir(exe).display().to_string());
    match data {
        Some(data) => crate::taste::fill_prompt(&text, data, exe),
        None => text,
    }
}
/// The edition's system prompt (docs/editions.md): the user's own, or the public one without his name, paths or projects.
#[cfg(not(feature = "public"))]
const PROMPT_FILE: &str = "prompt/heimdall.md";
#[cfg(not(feature = "public"))]
pub const PROMPT: &str = include_str!("../prompt/heimdall.md");
#[cfg(feature = "public")]
const PROMPT_FILE: &str = "prompt/public/heimdall.md";
#[cfg(feature = "public")]
pub const PROMPT: &str = include_str!("../prompt/public/heimdall.md");
/// The default system prompt for T3 tasks (docs/skills.md): the rules every brief carries, put at the top of each
/// `delegate_task` brief by the T3 layer (`t3::Workers::set_brief_preamble`), so Heimdall writes only the task.
#[cfg(not(feature = "public"))]
pub const WORKER_PROMPT: &str = include_str!("../prompt/worker.md");
#[cfg(feature = "public")]
pub const WORKER_PROMPT: &str = include_str!("../prompt/public/worker.md");
/// Pi's working folder: the home folder the user picked (docs/home.md), both editions; the user's own home folder if
/// it is missing, since a missing folder would stop Pi from starting at all.
fn working_dir() -> PathBuf {
    let home = heimdall_home::home_dir();
    if home.is_dir() {
        return home;
    }
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or(home)
}
/// The prompt with the tool log's real path in place of `tool_log::PROMPT_PLACEHOLDER`.
fn with_tool_log(prompt: &str, log: Option<&std::path::Path>) -> String {
    let path = log.map(|p| p.display().to_string()).unwrap_or_else(|| "logs/tools.log in Heimdall's data folder".into());
    prompt.replace(crate::tool_log::PROMPT_PLACEHOLDER, &path)
}
/// The prompt's name for the connectors guide, replaced at launch by `connectors_doc`.
pub const CONNECTORS_PLACEHOLDER: &str = "HEIMDALL_CONNECTORS_DOC";
/// The connectors guide (docs/connectors.md): the copy bundled in the installed app's Resources (both editions), or the
/// checkout's copy for a development or test binary.
pub fn connectors_doc(exe: &std::path::Path) -> PathBuf {
    match exe.parent().and_then(std::path::Path::parent) {
        Some(contents) if crate::runtime::in_app_bundle(exe) => contents.join("Resources/docs/connectors.md"),
        _ => crate::runtime::checkout_root().join("docs/connectors.md"),
    }
}
/// The prompt's name for the emergency-mode protocol (bug note 2026-10-27T03-22: a fixed checkout path pointed at a
/// branch without the file). The bundle's copy (`Contents/Resources/emergency.md`, this build's), else the checkout's.
pub const EMERGENCY_PLACEHOLDER: &str = "HEIMDALL_EMERGENCY_DOC";
pub fn emergency_doc(exe: &std::path::Path) -> PathBuf {
    match exe.parent().and_then(std::path::Path::parent) {
        Some(contents) if crate::runtime::in_app_bundle(exe) => contents.join("Resources/emergency.md"),
        _ => crate::runtime::checkout_root().join("docs/emergency.md"),
    }
}
/// The prompt's name for Heimdall's docs folder, indexed by its README.md ("if you need to know something
/// about yourself or models or modifying yourself or your own architecture you should go there to look").
pub const DOCS_PLACEHOLDER: &str = "HEIMDALL_DOCS_DIR";
/// The running build's docs: the bundle's `Contents/Resources/docs` when it holds the index (the personal edition,
/// scripts/bundle.sh), else the checkout's `docs/`.
pub fn docs_dir(exe: &std::path::Path) -> PathBuf {
    match exe.parent().and_then(std::path::Path::parent) {
        Some(contents) if crate::runtime::in_app_bundle(exe) && contents.join("Resources/docs/README.md").is_file() => {
            contents.join("Resources/docs")
        }
        _ => {
            let checkout = crate::runtime::checkout_root().join("docs");
            checkout.canonicalize().unwrap_or(checkout)
        }
    }
}
/// The prompt with the connectors guide's real path in place of `CONNECTORS_PLACEHOLDER`.
fn with_connectors_doc(prompt: &str, doc: &std::path::Path) -> String {
    prompt.replace(CONNECTORS_PLACEHOLDER, &doc.display().to_string())
}
struct Wake {
    id: String,
    consumed: bool,
    accepted: bool,
}
/// Shared with the extension; thresholds and user-facing notice have one source.
#[derive(serde::Deserialize)]
pub struct LoopPolicy {
    #[serde(rename = "identicalResults")]
    pub identical_results: u64,
    #[serde(rename = "blockedAttempts")]
    pub blocked_attempts: u64,
    pub notice: String,
}
/// Night pass 20→21 Oct (P3): how Heimdall's compactions summarize. Pi appends it to its own summary prompt as
/// "Additional focus"; the text is `prompt/compaction.md`.
#[cfg(not(feature = "public"))]
pub const COMPACTION_INSTRUCTIONS: &str = include_str!("../prompt/compaction.md");
#[cfg(feature = "public")]
pub const COMPACTION_INSTRUCTIONS: &str = include_str!("../prompt/public/compaction.md");
/// Pi's `compact` command for every compaction Heimdall starts (maintenance, catch-up, supervisor).
pub fn compact_command(id: &str) -> Value {
    json!({"type":"compact","id":id,"customInstructions":crate::identity::names_only(COMPACTION_INSTRUCTIONS.trim(), &crate::identity::current())})
}
/// What Pi receives for one delivery: the delivery marker, then the local date and time at sending (night pass 20→21
/// Oct, P1: on 20 Oct the agent believed it was 19 Oct and wrote that into bug notes and briefs), then the text. The
/// date line sits in the user message, never the system prompt, so the cached prefix stays the same all day; a resend
/// is stamped again when it goes out.
pub fn marked_prompt(id: &str, text: &str, now_ms: u64) -> String {
    format!("[Heimdall delivery: {id}]\n{}\n{text}", crate::clock::now_line(now_ms))
}
pub fn loop_policy() -> &'static LoopPolicy {
    static POLICY: std::sync::LazyLock<LoopPolicy> = std::sync::LazyLock::new(||
        serde_json::from_str(include_str!("../runtime/loop-guard.json")).expect("bundled loop policy"));
    &POLICY
}

/// Exactly one process owns the session lock. RPC acknowledgements echo command IDs;
/// settlement has no ID. A delivery marker in each user message makes attribution
/// durable in the session while leaving the system prompt and tool declarations stable.
pub struct Pi {
    config: PiConfig,
    process: Process,
    _lock: Option<std::fs::File>,
    pending: VecDeque<Event>,
    wakes: VecDeque<Wake>,
    final_assistant: Option<Value>,
    final_answer_reported: bool,
    completion_deadline: Option<tokio::time::Instant>,
    completion_receipts: std::collections::HashSet<String>,
    last_progress: tokio::time::Instant,
    provider_wait: Option<tokio::time::Instant>,
    tool_active: bool,
    known_tools: std::collections::HashSet<String>,
    running_tools: std::collections::HashSet<String>,
    stopped: bool,
    maintenance: bool,
    maintenance_id: Option<String>,
    /// The running maintenance compaction is optional (idle compaction): a failure is reported as an event, not a
    /// transport error, so the supervisor never restarts Pi over it.
    maintenance_optional: bool,
    compaction_started: bool,
    compaction_ended: bool,
    compacting: bool,
    entries_cursor: Option<String>,
    /// Extension commands this process offers, read once (every start and context notice needs them).
    commands: Option<Vec<String>>,
    loop_blocks: std::collections::HashSet<String>,
    loop_abort: Option<Value>,
    loop_stopped: bool,
    stop_generation: Option<u64>,
    stop_reason: Option<String>,
    tool_log: Option<crate::tool_log::ToolLog>,
    /// Deliveries of a turn `rejoin` ended: they complete without error when it settles (night pass H2).
    joining: std::collections::HashSet<String>,
    /// The running turn started writing a reply or a tool call (a text or tool-call block streams): it has acted, so
    /// the user's words steer it instead of restarting it (night review 4).
    streamed: bool,
    /// The user's Stop came during a compaction: the turn stops as the compaction ends, never cutting it.
    stop_after_compaction: bool,
    /// The last request that carried the history (`maintenance::LastRequest`); `None` after a compaction.
    last_request: Option<(u64, String, String)>,
}
impl Pi {
    pub fn launch(
        executable: &std::path::Path,
        session: &std::path::Path,
        cwd: &std::path::Path,
    ) -> Result<Self> {
        Self::launch_config(PiConfig::fixture(
            executable.into(),
            session.into(),
            cwd.into(),
        ))
    }
    pub fn launch_config(config: PiConfig) -> Result<Self> {
        Self::launch_config_permitted(config, None)
    }
    pub fn launch_config_permitted(config: PiConfig, permit: Option<crate::delivery::DeliveryPermit>) -> Result<Self> {
        if let Some(p) = &permit { p.check()?; }
        let stop_generation = permit.as_ref().map(|p| p.generation());
        use fs2::FileExt;
        use std::os::fd::AsRawFd;
        crate::runtime::private_dir(config.session.parent().context("session parent")?)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(config.session.parent().unwrap().join("pi-owner.lock"))?;
        // A concurrently forked child can briefly hold CLOEXEC descriptors until
        // exec. Bound that transient without allowing a second session owner.
        let lock_deadline=std::time::Instant::now()+std::time::Duration::from_millis(150);
        loop {match lock.try_lock_exclusive(){Ok(())=>break,Err(e)=>{if std::time::Instant::now()>=lock_deadline{return Err(e).context("another Pi still owns this session");}std::thread::sleep(std::time::Duration::from_millis(5));}}}
        // Child inheritance holds the OS lock across a parent crash/relaunch. The
        // session cannot reopen until the orphan exits and its tool children stop.
        // Every start, before Pi reads the history: an image the provider would refuse is repaired with Pi's own
        // append-only edit (20 Oct 19:20: one mislabelled screenshot failed every request after it; `history`).
        // A failed check never blocks the start.
        let repaired = match crate::history::repair(&config.session) {
            Ok(r) => r,
            Err(e) => { eprintln!("Agent history not checked: {e:#}"); None }
        };
        let entries_cursor = last_entry_id(&config.session)?;
        let last_request = crate::session::last_request(&config.session).unwrap_or_else(|e| { eprintln!("Pi session's last request unreadable: {e:#}"); None });
        let process = config.spawn(lock.as_raw_fd(), permit);
        let tool_log = config.tool_log.clone().map(crate::tool_log::ToolLog::new);
        let pending = repaired.map(|r| Event::Other(json!({"type":"heimdall_history_repair","state":"repaired_at_start","line":r.line()}))).into_iter().collect();
        Ok(Self {
            tool_log,
            commands: None,
            config,
            process: process?,
            _lock: Some(lock),
            pending,
            wakes: VecDeque::new(),
            final_assistant: None,
            final_answer_reported: false,
            completion_deadline: None,
            completion_receipts: Default::default(),
            last_progress: tokio::time::Instant::now(),
            provider_wait: None,
            tool_active: false,
            known_tools: Default::default(),
            running_tools: Default::default(),
            stopped: false,
            maintenance: false,
            maintenance_id: None,
            maintenance_optional: false,
            compaction_started: false,
            compaction_ended: false,
            compacting: false,
            entries_cursor,
            loop_blocks: Default::default(),
            loop_abort: None,
            loop_stopped: false,
            stop_generation,
            stop_reason: None,
            joining: Default::default(),
            streamed: false,
            stop_after_compaction: false,
            last_request,
        })
    }
    pub fn pid(&self) -> Option<u32> {
        crate::process_owner::Identity::read(self.process.pi_identity.pid).filter(|id|*id==self.process.pi_identity).map(|id|id.pid as u32)
    }
    pub fn cleanup_limited(&self) -> bool { self.process.owner.cleanup_limited }
    pub fn active(&self) -> bool {
        !self.wakes.is_empty() || self.maintenance
            // Final text is emitted before the per-delivery completions. Keep
            // reload/compaction parked until the caller consumes those receipts.
            || self.pending.iter().any(|event| matches!(event,Event::Complete {query:true,..}|Event::Deferred {..}))
    }
    pub fn compacting(&self) -> bool {
        self.compacting
    }
    /// A compaction Heimdall asked for, or one Pi reported starting and not yet ending. The user's Stop never cuts it
    /// (`abort`) and Agent off waits for it (`Process::watch_permit`, the supervisor). A prompt Pi refused as
    /// "compacting" with no compaction seen does not count, so it cannot hold a stop forever.
    pub fn compaction_running(&self) -> bool {
        self.maintenance || (self.compacting && self.compaction_started && !self.compaction_ended)
    }
    /// How long Pi has written nothing: what the compaction limit (`compaction_limit`) is measured against.
    pub fn silent_for(&self) -> std::time::Duration { self.last_progress.elapsed() }
    /// How long a compaction may stay silent before `next` ends Pi (`PiConfig::compaction_inactivity`).
    pub fn compaction_limit(&self) -> std::time::Duration { self.config.compaction_inactivity }
    /// `compaction_running`, or a compaction Pi reported that the bridge has not taken in yet: what Agent off waits
    /// for, so a Pi compaction is never cut for being read a moment late.
    pub fn compaction_in_progress(&self) -> bool {
        self.compaction_running() || self.process.compaction_reported.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Takes in the events Pi already wrote and the reader already holds, without waiting (as `rejoin` does): Agent
    /// off decides on the compaction Pi has reported, not on how far the bridge has read. They reach `next` as usual.
    pub fn take_in_ready(&mut self) -> Result<()> {
        while let Some(v) = self.process.try_next()? { self.ingest(v)?; }
        Ok(())
    }
    fn publish_compaction(&self) {
        self.process.compacting.store(self.compaction_running(), std::sync::atomic::Ordering::Release);
    }
    pub fn session_path(&self) -> &std::path::Path {
        &self.config.session
    }
    pub fn set_transport_recorder(&mut self, r: TransportRecorder) {
        self.process.set_recorder(Some(r));
    }
    pub fn set_delivery_gate(&mut self, g: Arc<crate::delivery::DeliveryGate>) {
        // Existing callers attach a gate to fixture launches. Production carries
        // its original permit into launch_config_permitted before admission.
        if let Ok(permit) = g.permit() {
            self.stop_generation = Some(permit.generation());
            self.process.watch_permit(permit);
        } else {
            self.process.kill_owned();
            self.process.gate = Some(g);
        }
    }
    async fn write(
        &mut self,
        v: Value,
        permit: Option<crate::delivery::DeliveryPermit>,
    ) -> Result<()> {
        let permit = match permit { Some(p)=>Some(p), None=>self.process.permit()? };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.process.send_permitted(v, permit),
        )
        .await;
        match result {
            Ok(result) => result,
            Err(_) => {
                self.process.kill_owned();
                anyhow::bail!("Pi input stalled; delivery may be partial");
            }
        }
    }
    async fn send_permitted(
        &mut self,
        v: Value,
        permit: Option<crate::delivery::DeliveryPermit>,
    ) -> Result<()> {
        if v["delivery_epoch"].as_u64().is_some_and(|epoch|permit.as_ref().map(|p|p.generation())!=Some(epoch)) {
            return Err(crate::delivery::DeliveryBlocked{written:0,total:0}.into());
        }
        if let (Some(gate), Some(permit)) = (&self.process.gate, &permit) {
            anyhow::ensure!(
                permit.belongs_to(gate),
                "delivery permit belongs to another gate"
            );
            permit
                .check()
                .map_err(|_| crate::delivery::DeliveryBlocked {
                    written: 0,
                    total: 0,
                })?;
        }
        anyhow::ensure!(
            !self.stopped && !self.process.gone()?,
            "Pi exited; supervisor must reconcile before restarting"
        );
        anyhow::ensure!(
            v["shouldQuery"] == true,
            "Pi observations stay in the durable local spool"
        );
        if let Some(engine) = &self.config.engine {
            let known=self.config.engine_signal.as_ref().map(|s|s.load(std::sync::atomic::Ordering::Acquire)).unwrap_or(0);
            // The manager's Ready signal can precede an asynchronous Off update.
            // Recheck at transport, after the bridge has durably spooled the wake.
            if known==1 || !engine.ready().await
                || self.config.engine_signal.as_ref().is_some_and(|s|s.load(std::sync::atomic::Ordering::Acquire)==1) {
                return Err(EngineUnavailable.into());
            }
        }
        if let Some(reason) = self.input_blocked() { return Err(InputUnavailable(reason).into()); }
        let id = v["uuid"]
            .as_str()
            .context("delivery id missing")?
            .to_owned();
        anyhow::ensure!(
            !id.is_empty()
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "invalid delivery id"
        );
        let c = v["message"]["content"]
            .as_array()
            .context("message content missing")?;
        let text = c
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let images:Vec<_>=c.iter().filter(|b|b["type"]=="image").map(|b|json!({"type":"image","data":b["source"]["data"],"mimeType":b["source"]["media_type"]})).collect();
        let marked = marked_prompt(&id, &text, crate::now_ms());
        if self.wakes.is_empty() && !self.maintenance {
            self.compaction_started=false;self.compaction_ended=false;self.publish_compaction();
        }
        self.wakes.push_back(Wake {
            id: id.clone(),
            consumed: false,
            accepted: false,
        });
        self.last_progress = tokio::time::Instant::now();
        self.write(json!({"id":id,"type":"prompt","message":marked,"observation_ids":crate::protocol::observation_ids(&v),"images":images,"streamingBehavior":"steer"}),permit).await
    }
    /// The thinking level from the next request on, without restarting the conversation (Pi's `set_thinking_level`;
    /// Pi clamps it to what the model takes). Returns the level Pi now runs.
    pub async fn set_thinking(&mut self, level: &str) -> Result<String> {
        anyhow::ensure!(crate::provider::THINKING_ORDER.contains(&level), "unknown thinking level {level}");
        self.query(json!({"type":"set_thinking_level","level":level})).await?;
        let state = self.query(json!({"type":"get_state"})).await?;
        Ok(state["thinkingLevel"].as_str().unwrap_or(level).to_owned())
    }
    /// Non-prompt commands have unique IDs and a bounded response. Session events
    /// consumed while awaiting a command are retained for the delivery ledger.
    pub async fn query(&mut self, command: Value) -> Result<Value> {
        let result = self.control(command).await?;
        self.enforce_loop_guard().await?;
        Ok(result)
    }
    async fn control(&mut self, mut command: Value) -> Result<Value> {
        let id = format!("control-{}", uuid::Uuid::new_v4());
        command["id"] = json!(id);
        self.write(command, None).await?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let v = self
                    .process
                    .next()
                    .await?
                    .context("Pi exited during control request")?;
                if v["type"] == "response" && v["id"] == id {
                    anyhow::ensure!(v["success"] == true, "Pi control failed: {}", v["error"]);
                    return Ok(v["data"].clone());
                }
                self.ingest(v)?;
            }
        })
        .await;
        match result {
            Ok(r) => r,
            Err(_) => {
                self.process.kill_owned();
                anyhow::bail!("Pi control request stalled");
            }
        }
    }
    pub async fn stop_and_rotate(
        &mut self,
        archive: impl FnOnce(&[u8]) -> Result<()>,
    ) -> Result<bool> {
        self.stopped = true;
        self.process.shutdown().await?;
        let result =
            crate::session::rotate(&self.config.session, crate::session::ROTATE_BYTES, archive);
        self._lock.take();
        let rotated=result?;
        anyhow::ensure!(!self.config.session.metadata().is_ok_and(|m|m.len()>=crate::session::ROTATE_BYTES), "Session cannot yet be safely bounded; retained checkpoint is too large or missing");
        Ok(rotated)
    }
    pub async fn compact(&mut self) -> Result<()> {
        self.compact_with(false).await
    }
    /// An optional compaction (history near the threshold while idle): if Pi cannot compact (the provider is down,
    /// the summary fails), `next` yields `heimdall_compaction_failed` instead of an error.
    pub async fn compact_optional(&mut self) -> Result<()> {
        self.compact_with(true).await
    }
    /// Bug note 2026-10-28T19-41-00: what the last compaction would have summarised (its cut, the previous summary, the
    /// conversation as text), written by the extension to `path`. Local, no model call.
    pub async fn compaction_source(&mut self, path: &std::path::Path) -> Result<()> {
        self.extension_note(COMPACTION_SOURCE_COMMAND, &path.to_string_lossy()).await
    }
    /// An optional compaction whose result Heimdall made (`path`: a summary for the source's cut, or a cut): Pi writes
    /// its own entry from it; a branch that changed since the source cancels it (`heimdall_compaction_failed`).
    pub async fn compact_supplied(&mut self, path: &std::path::Path) -> Result<()> {
        self.extension_note(COMPACTION_SUPPLY_COMMAND, &path.to_string_lossy()).await?;
        self.compact_optional().await
    }
    async fn compact_with(&mut self, optional: bool) -> Result<()> {
        anyhow::ensure!(!self.active(), "compaction waits for settlement");
        self.maintenance = true;
        self.maintenance_optional = optional;
        let id=format!("maintenance-{}",uuid::Uuid::new_v4());
        self.maintenance_id=Some(id.clone());
        self.compaction_started=false;self.compaction_ended=false;
        self.last_progress = tokio::time::Instant::now();
        let sent = self.write(compact_command(&id), None).await;
        if sent.is_err() {
            // Nothing asked Pi to compact (Off revoked the write): no compaction to wait for or show.
            self.maintenance = false; self.maintenance_id = None; self.maintenance_optional = false;
        }
        self.publish_compaction();
        sent
    }
    /// The running turn has neither acted nor replied: every delivery in it reached the model, no assistant message
    /// has ended (no tool call decided, no reply written), none is being written (no text or tool-call block has
    /// started streaming), no tool runs, and nothing is settling or compacting.
    pub fn can_rejoin(&self) -> bool {
        !self.maintenance && !self.compacting && self.stop_reason.is_none() && self.loop_abort.is_none() && !self.loop_stopped
            && !self.wakes.is_empty() && self.wakes.iter().all(|w| w.consumed)
            && self.final_assistant.is_none() && !self.final_answer_reported && !self.streamed
            && self.known_tools.is_empty() && self.running_tools.is_empty() && !self.tool_active
            && self.completion_receipts.is_empty() && self.completion_deadline.is_none()
            && !self.pending.iter().any(|event| matches!(event, Event::Complete { query: true, .. } | Event::Deferred { .. }))
    }
    /// Ends the turn so the user's next input restarts it (`Harness::rejoin`), unless it has acted or replied by now.
    /// Events Pi already wrote are taken in first: the bridge may not have read a tool call or reply that started
    /// streaming (night review 4), and an abort then would cut it. Pi drops an aborted assistant message from the
    /// next request, so the restarted request carries the earlier words and the new ones, and nothing else.
    pub async fn rejoin(&mut self) -> Result<bool> {
        // Let the output reader hand over what it has read, then take in everything waiting.
        tokio::task::yield_now().await;
        while let Some(v) = self.process.try_next()? {
            self.ingest(v)?;
        }
        if !self.can_rejoin() { return Ok(false); }
        self.joining.extend(self.wakes.iter().map(|w| w.id.clone()));
        let result = self.control(json!({"type":"abort"})).await;
        if result.is_err() { self.joining.clear(); }
        result.map(|_| true)
    }
    /// `maintenance::LastRequest` for the model this process runs.
    pub fn last_request(&self) -> Option<crate::maintenance::LastRequest> {
        let (at_ms, provider, model) = self.last_request.as_ref()?;
        let (running_provider, running_model) = self.config.selected_model();
        Some(crate::maintenance::LastRequest { at_ms: *at_ms, same_model: provider.as_str() == running_provider && model.as_str() == running_model })
    }
    /// The user's Stop. A compaction is never cut ("When the agent is compacting I should not be able to
    /// interrupt you"; Pi's `abort` would throw the summary away after its input was billed): Heimdall's own compaction
    /// has no turn to stop and runs on, and a turn compacting inside its run stops as the compaction ends (`next`).
    pub async fn abort(&mut self) -> Result<()> {
        // Only a compaction Pi reported starting defers it: a prompt Pi refused as "compacting" with no compaction
        // seen must not hold a Stop forever.
        if self.compaction_running() {
            self.stop_after_compaction = !self.wakes.is_empty();
            return Ok(());
        }
        self.loop_abort = None;
        self.stop_after_compaction = false;
        self.abort_with_reason("Stopped by User").await
    }
    async fn abort_with_reason(&mut self, reason: &str) -> Result<()> {
        self.stop_reason = Some(reason.into());
        // abort alone resumes queued follow-ups. Clear BOTH queues first.
        self.control(json!({"type":"clear_queue"})).await?;
        self.control(json!({"type":"abort"})).await?;
        for wake in self.wakes.drain(..) {
            // An abort ends the turn on purpose (the user's Stop, a reload that aborts, the loop guard): the bridge
            // settles it and never resends it.
            self.pending.push_back(Event::Other(json!({"type":"heimdall_aborted","id":wake.id,"reason":reason})));
            self.pending.push_back(Event::Complete {
                id: Some(wake.id),
                query: true,
                error: true,
                text: reason.into(),
            });
        }
        self.stop_reason = None;
        self.completion_deadline = None;
        self.completion_receipts.clear();
        self.loop_blocks.clear();
        self.loop_stopped = false;
        self.streamed = false;
        Ok(())
    }
    async fn enforce_loop_guard(&mut self) -> Result<()> {
        if let Some(mut event) = self.loop_abort.clone() {
            // Retain the request across await: next() may be cancelled by the host's select.
            // Clearing queues and aborting are idempotent if a cancelled control needs retry.
            // Events drained by either command cannot schedule another abort.
            self.loop_stopped = true;
            self.abort_with_reason(&loop_policy().notice).await?;
            self.loop_abort = None;
            event["type"] = json!("heimdall_loop_stopped");
            event["text"] = json!(loop_policy().notice);
            event["delivery_epoch"] = json!(self.stop_generation);
            self.pending.push_back(Event::Other(event));
        }
        Ok(())
    }
    pub async fn state(&mut self) -> Result<Value> {
        self.query(json!({"type":"get_state"})).await
    }
    /// Pi's estimate of the context the next request carries, and the window it measures against. `None` tokens:
    /// unknown until the first answer after a compaction (the history is then the summary and the kept tail).
    pub async fn context_usage(&mut self) -> Result<(Option<u64>, u64)> {
        let stats = self.query(json!({"type":"get_session_stats"})).await?;
        let usage = &stats["contextUsage"];
        Ok((usage["tokens"].as_u64(), usage["contextWindow"].as_u64().unwrap_or(0)))
    }
    /// Adds `text` to the conversation as context only: a durable custom message in the same session, no turn,
    /// no reply. Refused (nothing sent) unless the runtime's extension provides the command, because an unknown
    /// slash command would reach the model as an ordinary request.
    pub async fn append_context(&mut self, text: &str) -> Result<()> {
        self.extension_note(CONTEXT_COMMAND, text).await
    }
    /// Records the reload outcome in the conversation (shown in the Agent window), without a turn.
    pub async fn note_reload(&mut self, text: &str) -> Result<()> {
        self.extension_note(RELOAD_NOTE_COMMAND, text).await
    }
    async fn extension_note(&mut self, command: &str, text: &str) -> Result<()> {
        anyhow::ensure!(!self.active(), "notes wait for settlement");
        if self.commands.is_none() {
            let listed = self.query(json!({"type":"get_commands"})).await?;
            self.commands = Some(listed["commands"].as_array().into_iter().flatten().filter_map(|c| c["name"].as_str().map(str::to_owned)).collect());
        }
        anyhow::ensure!(
            self.commands.as_ref().is_some_and(|c| c.iter().any(|c| c == command)),
            "Pi runtime cannot add a note without a turn"
        );
        let r = self.query(json!({"type":"prompt","message":format!("/{command} {text}")})).await?;
        anyhow::ensure!(r["disposition"] == "handled", "Pi did not add the note");
        Ok(())
    }
    pub async fn entries(&mut self, since: Option<&str>) -> Result<Value> {
        let mut v = json!({"type":"get_entries"});
        if let Some(s) = since {
            v["since"] = json!(s);
        }
        self.query(v).await
    }
    pub async fn incremental_entries(&mut self) -> Result<Value> {
        let since = self.entries_cursor.clone();
        let entries = self.entries(since.as_deref()).await?;
        if let Some(id) = entries["entries"].as_array().and_then(|a|a.last()).and_then(|v|v["id"].as_str()) {self.entries_cursor=Some(id.into());}
        Ok(entries)
    }
    /// Pi's fork creates a new session file. Persist the returned path in the app
    /// before any edited prompt is sent; the original branch remains archived.
    pub async fn fork(&mut self, entry_id: &str) -> Result<std::path::PathBuf> {
        anyhow::ensure!(
            !self.active(),
            "stop the current turn before editing an earlier message"
        );
        let messages = self.query(json!({"type":"get_fork_messages"})).await?;
        anyhow::ensure!(
            messages["messages"]
                .as_array()
                .is_some_and(|a| a.iter().any(|m| m["entryId"] == entry_id)),
            "message is not on the active branch"
        );
        let result = self
            .query(json!({"type":"fork","entryId":entry_id}))
            .await?;
        anyhow::ensure!(result["cancelled"] != true, "fork cancelled");
        let state = self.state().await?;
        let path = PathBuf::from(
            state["sessionFile"]
                .as_str()
                .context("fork session path missing")?,
        );
        let parent = self.config.session.parent().context("session parent")?;
        anyhow::ensure!(path.starts_with(parent), "fork moved outside Heimdall data");
        self.entries_cursor = last_entry_id(&path)?;
        self.config.session = path.clone();
        Ok(path)
    }
    /// A receipt is durable proof of this exact delivery, even if the final RPC
    /// settlement frame is lost. Never infer completion from agent_end alone.
    fn recover_completed(&mut self) -> Result<()> {
        self.completion_deadline = None;
        let mut seen = std::collections::HashSet::new();
        for receipt in crate::session::receipts(&self.config.session)?.into_iter().rev() {
            if seen.insert(receipt.id.clone()) && self.wakes.iter().any(|w|w.id == receipt.id) {
                if receipt.completed { self.completion_receipts.insert(receipt.id); }
                else { self.completion_receipts.remove(&receipt.id); }
            }
        }
        let text = self.final_assistant.as_ref().and_then(|m|m["content"].as_array())
            .into_iter().flatten().filter(|p| p["type"] == "text").filter_map(|p|p["text"].as_str()).collect::<Vec<_>>().join("\n");
        let pending_start = self.pending.len();
        let mut recovered = vec![];
        self.wakes.retain(|w| {
            if self.completion_receipts.remove(&w.id) {
                recovered.push(w.id.clone());
                self.pending.push_back(Event::Complete { id: Some(w.id.clone()), query: true, error: false, text: text.clone() });
                false
            } else { true }
        });
        if !recovered.is_empty() {
            if !self.final_answer_reported {
                if let Some(message) = &self.final_assistant {
                    if let Some(event) = final_answer_event(message) {
                        self.pending.insert(pending_start, event);
                        self.final_answer_reported = true;
                    }
                }
            }
            if !self.wakes.iter().any(|w|w.consumed) {
                self.final_assistant = None;
                self.tool_active = false; self.running_tools.clear(); self.known_tools.clear();
                self.loop_blocks.clear();
                self.streamed = false;
            }
            self.pending.push_back(Event::Other(json!({"type":"heimdall_settlement_recovered","ids":recovered,"reason":"successful receipt without settlement within 2 seconds"})));
        }
        Ok(())
    }
    fn ingest(&mut self, v: Value) -> Result<()> {
        let result = self.ingest_event(v);
        self.publish_compaction();
        result
    }
    fn ingest_event(&mut self, v: Value) -> Result<()> {
        if let Some(log) = &mut self.tool_log { log.observe(&v, crate::now_ms()); }
        if self.config.api_model.is_some() && v["type"] == "heimdall_provider_wait" {
            let delay = v["delay_ms"].as_u64().unwrap_or(0).min(86400000);
            self.provider_wait = (delay > 0).then(||tokio::time::Instant::now()+std::time::Duration::from_millis(delay));
            self.last_progress = tokio::time::Instant::now();
        }

        let progress = self.active() && match v["type"].as_str() {
            Some("response") => (v["command"] == "prompt" && self.wakes.iter().any(|w| !w.accepted && v["id"] == w.id))
                || (self.maintenance && v["command"] == "compact" && v["id"].as_str()==self.maintenance_id.as_deref()),
            Some("message_update") => self.wakes.iter().any(|w|w.consumed) && v["assistantMessageEvent"]["type"].as_str().is_some_and(|t|matches!(t,"text_delta"|"thinking_delta"|"toolcall_delta")) && v["assistantMessageEvent"]["delta"].as_str().is_some_and(|s|!s.is_empty()),
            Some("message_end" | "turn_end") => v["message"]["role"] == "assistant" && self.wakes.iter().any(|w|w.consumed) && v["message"]["content"].is_array() && v["message"]["stopReason"].is_string() && self.final_assistant.as_ref()!=Some(&v["message"]),
            Some("tool_execution_start") => self.wakes.iter().any(|w|w.consumed) && v["toolCallId"].as_str().is_some_and(|s|self.known_tools.contains(s)&&!self.running_tools.contains(s)),
            Some("tool_execution_end") => v["toolCallId"].as_str().is_some_and(|s|self.running_tools.contains(s)),
            Some("tool_execution_update") => v["toolCallId"].as_str().is_some_and(|s|self.running_tools.contains(s)) && v["partialResult"]["content"].as_array().is_some_and(|a|a.iter().any(|v|v["text"].as_str().is_some_and(|s|!s.is_empty()))),
            Some("compaction_start") => !self.compaction_started && (
                (self.maintenance && v["id"].as_str().is_none_or(|id|Some(id)==self.maintenance_id.as_deref()))
                // "manual" with a wake pending is the extension's cold-cache compaction before that wake's turn.
                || (!self.maintenance && !self.wakes.is_empty() && v["reason"].as_str().is_some_and(|s|matches!(s,"threshold"|"overflow"|"manual")))),
            Some("compaction_end") => self.compaction_started && !self.compaction_ended && (
                (self.maintenance && v["id"].as_str().is_none_or(|id|Some(id)==self.maintenance_id.as_deref()))
                || (!self.maintenance && self.wakes.iter().any(|w|w.consumed))),
            _ => false,
        };
        if progress { self.last_progress = tokio::time::Instant::now(); }
        match v["type"].as_str() {
            Some("response")
                if v["command"] == "compact"
                    && self.maintenance
                    && v["id"].as_str()==self.maintenance_id.as_deref() =>
            {
                self.maintenance = false;
                self.maintenance_id = None;
                self.compacting = false;
                let optional = std::mem::take(&mut self.maintenance_optional);
                if v["success"] != true && optional {
                    self.pending.push_back(Event::Other(json!({"type":"heimdall_compaction_failed","error":v["error"]})));
                } else {
                    anyhow::ensure!(v["success"] == true, "Pi could not compact the session");
                }
            }
            Some("compaction_start") if progress => {
                self.compaction_started = true;
                self.compacting = true;
            }
            Some("compaction_end") if progress || self.compacting => {
                self.compaction_ended = true;
                self.compacting = false;
            }
            _ => {}
        }
        // The provider's cache now holds the history up to this request (`maintenance`); a finished compaction
        // replaces the history, so nothing cached is worth keeping warm until the next request.
        match v["type"].as_str() {
            Some("message_end") if v["message"]["role"] == "assistant" => {
                let usage = &v["message"]["usage"];
                let prompt = ["input", "cacheRead", "cacheWrite"].iter().map(|k| usage[*k].as_u64().unwrap_or(0)).sum::<u64>();
                if prompt > 0 {
                    let at = v["message"]["timestamp"].as_u64().unwrap_or_else(crate::now_ms);
                    self.last_request = Some((at, v["message"]["provider"].as_str().unwrap_or_default().into(), v["message"]["model"].as_str().unwrap_or_default().into()));
                }
            }
            Some("compaction_end") if v["aborted"] != true && !v["result"].is_null() => self.last_request = None,
            _ => {}
        }
        match v["type"].as_str() {
            Some("response") if v["command"] == "prompt" && self.wakes.iter().any(|w|v["id"] == w.id) => {
                let id = v["id"]
                    .as_str()
                    .context("Pi prompt response missing id")?
                    .to_owned();
                if v["success"] == true {
                    if let Some(w) = self.wakes.iter_mut().find(|w| w.id == id) {
                        w.accepted = true;
                    }
                    self.pending.push_back(Event::Echo { id: id.clone() });
                    if v["data"]["disposition"] == "handled" {
                        self.wakes.retain(|w| w.id != id);
                        self.pending.push_back(Event::Complete {
                            id: Some(id),
                            query: true,
                            error: false,
                            text: String::new(),
                        });
                    }
                } else if v["error"].as_str().is_some_and(|error|error.starts_with("Cannot submit a prompt while compaction is in progress."))
                    && self.wakes.iter().any(|w|w.id == id && !w.consumed && !w.accepted) {
                    self.wakes.retain(|w| w.id != id);
                    self.compacting = true;
                    self.pending.push_back(Event::Deferred { id, reason:"Pi is compacting the session".into() });
                } else {
                    self.wakes.retain(|w| w.id != id);
                    self.pending.push_back(Event::Complete {
                        id: Some(id),
                        query: true,
                        error: true,
                        text: v["error"]
                            .as_str()
                            .unwrap_or("Pi rejected the prompt")
                            .into(),
                    });
                }
            }
            // A text or tool-call block started streaming in the running turn: it has acted (night review 4: a restart
            // then would cut a reply or a tool call being written). Thinking does not count.
            Some("message_update") if self.wakes.iter().any(|w| w.consumed)
                && v["assistantMessageEvent"]["type"].as_str().is_some_and(|t| t.starts_with("text_") || t.starts_with("toolcall_")) => {
                self.streamed = true;
            }
            Some("message_end") if v["message"]["role"] == "user" => {
                let c = &v["message"]["content"];
                let text = if let Some(s) = c.as_str() {
                    s.to_owned()
                } else {
                    c.as_array()
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                // Pi may append image coordinate notes; the marker is the
                // identity, while the remainder of the user text may change.
                if let Some(w) = self
                    .wakes
                    .iter_mut()
                    .find(|w| text.starts_with(&format!("[Heimdall delivery: {}]\n", w.id)) && !w.consumed)
                {
                    w.consumed = true;
                    self.pending.push_back(Event::Consumed { id: w.id.clone() });
                    self.last_progress = tokio::time::Instant::now();
                }
            }
            Some("message_end" | "turn_end") if v["message"]["role"] == "assistant" => {
                for part in v["message"]["content"].as_array().into_iter().flatten() {
                    if part["type"]=="toolCall" {if let Some(id)=part["id"].as_str(){self.known_tools.insert(id.into());}}
                }
                if self.final_assistant.as_ref() != Some(&v["message"]) {
                    self.final_answer_reported = false;
                }
                self.final_assistant = Some(v["message"].clone());
                // A finished reply is said when its turn ends, not when the run settles: Pi keeps one run across
                // steering, so a steer that arrives while this reply streams starts another turn in the same run,
                // and only the run's last message would reach settlement (review 19 Oct, MUST 1). Aborted and
                // failed replies still report at settlement.
                if v["type"] == "turn_end" && !self.final_answer_reported
                    && matches!(v["message"]["stopReason"].as_str(), Some("stop" | "length")) {
                    if let Some(event) = final_answer_event(&v["message"]) {
                        self.pending.push_back(event);
                        self.final_answer_reported = true;
                    }
                }
            }
            Some("tool_execution_start") if progress => {
                self.running_tools.insert(v["toolCallId"].as_str().unwrap().into());
                self.tool_active = true;
            }
            Some("tool_execution_end") if progress => {
                self.running_tools.remove(v["toolCallId"].as_str().unwrap());
                self.tool_active = !self.running_tools.is_empty();
            }
            Some("entry_appended") if v["entry"]["customType"] == "heimdall_loop_guard"
                && self.active() && self.wakes.iter().any(|w| w.consumed) && !self.loop_stopped => {
                let entry = &v["entry"];
                let data = &entry["data"];
                if let Some(id) = entry["id"].as_str() {
                    if data["repeats"].as_u64().is_some_and(|n| n >= loop_policy().identical_results)
                        && data["tool"].is_string() && data["arguments_hash"].is_string()
                        && self.loop_blocks.insert(id.into())
                        && self.loop_blocks.len() as u64 >= loop_policy().blocked_attempts {
                        let mut detail = data.clone();
                        detail["blocked_attempts"] = json!(self.loop_blocks.len());
                        detail["entry_id"] = json!(id);
                        self.loop_abort = Some(detail);
                    }
                }
            }
            // A step of the in-run history repair (`runtime/history-repair.mjs`) is a session entry Pi announces on
            // its own output; the log, island and lab checks read it as one event, like the start's repair below.
            Some("entry_appended") if v["entry"]["customType"] == "heimdall_history_repair" => {
                let data = &v["entry"]["data"];
                self.pending.push_back(Event::Other(json!({"type":"heimdall_history_repair","state":data["state"],"line":data["line"]})));
            }
            Some("entry_appended") if v["entry"]["customType"] == "heimdall_receipt"
                && v["entry"]["data"]["completed"] == true => {
                if let Some(id) = v["entry"]["data"]["id"].as_str() {
                    if self.wakes.iter().any(|w| w.id == id) {
                        self.completion_receipts.insert(id.into());
                        self.completion_deadline.get_or_insert(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
                    }
                }
            }
            Some("agent_end") if v["willRetry"] == false && !self.wakes.is_empty() => {
                self.completion_deadline.get_or_insert(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
            }
            Some("agent_settled") => {
                self.completion_deadline = None;
                let final_ = self.final_assistant.take();
                let aborted = v["aborted"] == true;
                let error = self.stop_reason.is_some() || aborted
                    || final_.as_ref().is_none_or(|m| {
                        m["stopReason"] != "stop"
                            || m["errorMessage"].as_str().is_some_and(|s| !s.is_empty())
                    });
                let detail = self.stop_reason.as_deref().or_else(|| final_
                    .as_ref()
                    .and_then(|m| m["errorMessage"].as_str()))
                    .unwrap_or("Pi settled without a successful reply");
                let engine_error = error
                    && !aborted
                    && (detail.contains("503")
                        || detail.contains("ECONNREFUSED")
                        || detail.contains("fetch failed")
                        || detail.contains("terminated")
                        || detail.contains("overloaded"));
                let text = if error {
                    detail.to_string()
                } else {
                    final_
                        .as_ref()
                        .and_then(|m| m["content"].as_array())
                        .into_iter()
                        .flatten()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                // One final answer per assistant reply, independent of how many
                // wake/delivery receipts that reply completes. Never tool text.
                // A run that ends without one (aborted mid-tool, no message at
                // all) still closes the turn's reply state with an empty aborted
                // final, so its `speak` lines never leak into the next turn.
                if !self.final_answer_reported {
                    let event = final_.as_ref().and_then(final_answer_event).unwrap_or_else(||
                        Event::Other(json!({"type":"heimdall_final_answer","text":"","stop_reason":"aborted"})));
                    self.pending.push_back(event);
                }
                self.final_answer_reported = false;
                let joined = std::mem::take(&mut self.joining);
                let mut retain = VecDeque::new();
                for w in self.wakes.drain(..) {
                    let receipt_completed = self.completion_receipts.contains(&w.id);
                    if joined.contains(&w.id) {
                        // Ended by `rejoin`: its words are in the conversation and the restarted turn answers them.
                        self.pending.push_back(Event::Complete { id: Some(w.id), query: true, error: false, text: String::new() });
                    } else if w.consumed || receipt_completed {
                        if aborted && !engine_error && !receipt_completed {
                            self.pending.push_back(Event::Other(json!({"type":"heimdall_aborted","id":w.id,"reason":"aborted"})));
                        }
                        self.pending.push_back(if engine_error && !receipt_completed {
                            Event::Deferred {
                                id: w.id,
                                reason: "Agent waiting for the engine".into(),
                            }
                        } else {
                            Event::Complete {
                                id: Some(w.id),
                                query: true,
                                error: error && !receipt_completed,
                                text: text.clone(),
                            }
                        });
                    } else {
                        retain.push_back(w);
                    }
                }
                // No user marker is no proof of delivery: preserve all unresolved wakes.
                self.wakes = retain;
                self.completion_receipts.clear();
                self.tool_active = false;self.running_tools.clear();self.known_tools.clear();
                self.loop_blocks.clear();
                self.streamed = false;
                if self.loop_abort.is_none() { self.loop_stopped = false; }
            }
            Some("extension_error") => {
                anyhow::bail!("Pi extension failed; reload the agent");
            }
            Some("extension_ui_request") => {
                // An unhandled confirmation must never silently freeze or imply consent.
                if !matches!(
                    v["method"].as_str(),
                    Some("notify" | "setStatus" | "setWidget" | "setTitle" | "set_editor_text")
                ) {
                    anyhow::bail!("Pi requested an unsupported interactive dialog");
                }
            }
            _ => {}
        }
        self.pending.push_back(Event::Other(v));
        Ok(())
    }
}
#[async_trait]
impl Harness for Pi {
    fn max_text_bytes(&self) -> usize {
        self.config.max_text_bytes
    }
    fn history_end_ms(&self) -> Option<u64> {
        crate::session::history_end(&self.config.session).unwrap_or_else(|e| { eprintln!("Pi session's history end unreadable: {e:#}"); None })
    }
    fn serial_wakes(&self) -> bool {
        true
    }
    fn supports_steering(&self) -> bool { true }
    fn input_blocked(&self) -> Option<&'static str> {
        if self.compacting || self.maintenance { Some("Pi is compacting the session") } else { None }
    }
    fn delivery_epoch(&self) -> Option<u64> {
        self.process.gate.as_ref().map(|g| g.generation())
    }
    fn turn_active(&self) -> bool { self.active() }
    async fn append_context(&mut self, text: &str) -> Result<()> {
        if self.active() { return Err(InputUnavailable("Context waits for the turn to settle").into()); }
        self.extension_note(CONTEXT_COMMAND, text).await
    }
    async fn context_tokens(&mut self) -> Result<Option<(Option<u64>, u64)>> { Ok(Some(Pi::context_usage(self).await?)) }
    /// A start's compaction is optional: if Pi cannot summarise, `heimdall_compaction_failed` follows and the context
    /// is added anyway (Pi compacts on its own before the next request).
    async fn compact_history(&mut self) -> Result<()> { Pi::compact_optional(self).await }
    fn can_rejoin(&self) -> bool { Pi::can_rejoin(self) }
    async fn rejoin(&mut self) -> Result<bool> { Pi::rejoin(self).await }
    fn last_request(&self) -> Option<crate::maintenance::LastRequest> { Pi::last_request(self) }
    async fn send(&mut self, v: Value) -> Result<()> {
        let permit = self.process.permit()?;
        self.send_permitted(v, permit).await
    }
    async fn send_with_permit(
        &mut self,
        v: Value,
        p: crate::delivery::DeliveryPermit,
    ) -> Result<()> {
        self.send_permitted(v, Some(p)).await
    }
    async fn next(&mut self) -> Result<Option<Event>> {
        loop {
            self.enforce_loop_guard().await?;
            if self.stop_after_compaction && !self.compacting && !self.maintenance {
                self.abort().await?;
            }
            if self.completion_deadline.is_some_and(|d|tokio::time::Instant::now() >= d) {
                self.recover_completed()?;
            }
            if let Some(e) = self.pending.pop_front() {
                return Ok(Some(e));
            }
            let inactivity_deadline = self.last_progress
                + if self.compaction_running() {
                    self.config.compaction_inactivity
                } else if self.tool_active {
                    std::time::Duration::from_secs(600)
                } else {
                    self.config.inactivity
                };
            let inactivity_deadline = self.provider_wait.map_or(inactivity_deadline, |until|inactivity_deadline.max(until + self.config.inactivity));
            let deadline = self.completion_deadline.map_or(inactivity_deadline, |d|d.min(inactivity_deadline));
            if self.completion_deadline.is_some_and(|d|tokio::time::Instant::now() >= d) { continue; }
            if self.active() && tokio::time::Instant::now() >= deadline {anyhow::bail!("Pi made no progress during an active turn");}
            let v = if self.active() {
                match tokio::time::timeout_at(deadline, self.process.next()).await {
                    Ok(v) => v?,
                    Err(_) if self.completion_deadline.is_some_and(|d|tokio::time::Instant::now() >= d) => continue,
                    Err(_) => anyhow::bail!("Pi made no progress during an active turn"),
                }
            } else {
                self.process.next().await?
            };
            let Some(v) = v else {
                return Ok(None);
            };
            self.ingest(v)?;
        }
    }
    async fn shutdown(&mut self) -> Result<()> {
        self.stopped = true;
        let result = self.process.shutdown().await;
        self._lock.take();
        result
    }
}

#[cfg(test)]
mod ownership_regressions {
    use super::*;
    const ESCAPING_CHAIN:&str=include_str!("../tests/fixtures/escaping-tool.py");
    fn node_script(dir:&std::path::Path)->PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let preload=dir.join("tool-owner.cjs");std::fs::write(&preload,include_str!("../runtime/tool-owner.cjs")).unwrap();
        let js=dir.join("fixture.cjs");std::fs::write(&js,r#"
const fs=require('node:fs');const cp=require('node:child_process');
const child=cp.spawn('/bin/sh',['-c','sleep 60 & echo $! > sleep.pid; wait'],{detached:true,stdio:'ignore'});
fs.writeFileSync('tool.pid',String(child.pid));setInterval(()=>{},1000);
"#).unwrap();
        let script=dir.join("fixture.sh");std::fs::write(&script,format!("#!/bin/sh\nexec node --require '{}' '{}'\n",preload.display(),js.display())).unwrap();
        std::fs::set_permissions(&script,std::fs::Permissions::from_mode(0o700)).unwrap();script
    }
    async fn wait_file(path:&std::path::Path)->i32 {
        let start=std::time::Instant::now();loop {if let Ok(text)=std::fs::read_to_string(path){if let Ok(pid)=text.trim().parse(){return pid;}}assert!(start.elapsed()<std::time::Duration::from_secs(3));tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
    }
    async fn gone(pid:i32) {
        let start=std::time::Instant::now();while unsafe{libc::kill(pid,0)}==0 {assert!(start.elapsed()<std::time::Duration::from_secs(3),"survivor {pid}");tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
    }
    #[tokio::test]
    async fn stop_watcher_is_installed_before_admission_and_off_never_releases_tools() {
        use std::sync::atomic::Ordering;
        for back_on in [false,true] {
            let d=tempfile::tempdir().unwrap();let groups=d.path().join("groups");
            crate::runtime::private_dir(&groups).unwrap();let owner=crate::process_owner::Owner::new();
            let gate=Arc::new(crate::delivery::DeliveryGate::new(true));let permit=gate.permit().unwrap();let revoke=gate.clone();
            TEST_BEFORE_GUARDIAN_RELEASE.with(|hook|*hook.borrow_mut()=Some(Box::new(move|process|{
                assert!(process.stop_watch.is_some(),"child released without cancellation supervision");
                revoke.store(false,Ordering::Release);if back_on {revoke.store(true,Ordering::Release);}
            })));
            let result=Process::spawn_env_permitted(std::path::Path::new("/bin/sh"),
                &["-c".into(),"touch command-executed; sleep 60".into()],d.path(),&[
                    (crate::process_owner::TOKEN.into(),owner.token),
                    ("HEIMDALL_TOOL_GROUPS".into(),groups.to_string_lossy().into_owned())],Some(permit));
            assert!(result.is_err());assert!(!d.path().join("command-executed").exists());
            assert!(!groups.join("admitted").exists());assert!(groups.join("generation.retired").exists());
        }
    }

    #[tokio::test]
    async fn detached_tool_groups_and_descendants_die_on_off_and_abrupt_pi_death_before_replacement() {
        for off in [true,false] {
            let d=tempfile::tempdir().unwrap();let executable=node_script(d.path());let config=PiConfig::fixture(executable,d.path().join("session"),d.path().into());
            let mut pi=Pi::launch_config(config.clone()).unwrap();let gate=Arc::new(crate::delivery::DeliveryGate::new(true));pi.set_delivery_gate(gate.clone());
            let tool=wait_file(&d.path().join("tool.pid")).await;let descendant=wait_file(&d.path().join("sleep.pid")).await;
            assert_ne!(unsafe{libc::getpgid(tool)},pi.pid().unwrap() as i32);
            if off {gate.store(false,std::sync::atomic::Ordering::Release);}else{unsafe{libc::kill(pi.pid().unwrap() as i32,libc::SIGKILL);};assert!(pi.next().await.unwrap().is_none());}
            pi.shutdown().await.unwrap();gone(tool).await;gone(descendant).await;drop(pi);
            let mut next=Pi::launch_config(config).unwrap();next.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn escaping_setsid_and_double_fork_descendants_retire() {
        for off in [true,false] {
            let d=tempfile::tempdir().unwrap();
            let py=d.path().join("escape.py");
            std::fs::write(&py,ESCAPING_CHAIN).unwrap();
            let executable=node_script(d.path());
            std::fs::write(d.path().join("fixture.cjs"),r#"const fs=require('node:fs');const cp=require('node:child_process');cp.spawn('/bin/sh',['-c','exec /usr/bin/python3 escape.py'],{detached:true,stdio:'ignore'});setInterval(()=>{},1000);"#).unwrap();
            let mut pi=Pi::launch_config(PiConfig::fixture(executable,d.path().join("session"),d.path().into())).unwrap();
            let gate=Arc::new(crate::delivery::DeliveryGate::new(true));pi.set_delivery_gate(gate.clone());
            let detached=wait_file(&d.path().join("setsid.pid")).await;let double=wait_file(&d.path().join("double.pid")).await;
            if off {gate.store(false,std::sync::atomic::Ordering::Release);}else{unsafe{libc::kill(pi.pid().unwrap() as i32,libc::SIGKILL);};assert!(pi.next().await.unwrap().is_none());}
            pi.shutdown().await.unwrap();
            let survived=[detached,double].iter().any(|p|unsafe{libc::kill(*p,0)}==0);
            assert!(!survived,"setsid/double-fork descendant survived");
        }
    }
    /// 27 Oct 03:24-03:46: Pi is silent while the model summarises, and the turn's 180 s inactivity limit killed
    /// every Sol compaction of the 230k-token session. A running compaction gets its own, longer limit.
    #[tokio::test]
    async fn a_silent_compaction_outlives_the_turn_inactivity_limit() {
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        pi.config.inactivity=std::time::Duration::from_millis(60);
        pi.config.compaction_inactivity=std::time::Duration::from_secs(30);
        let hold=d.path().join("hold-compaction");std::fs::write(&hold,"").unwrap();
        pi.compact().await.unwrap();
        let until=tokio::time::Instant::now()+std::time::Duration::from_millis(600);
        while let Ok(r)=tokio::time::timeout_at(until,Harness::next(&mut pi)).await {
            assert!(r.is_ok(),"a silent compaction was killed: {:?}",r.err());
        }
        assert!(pi.compaction_running(),"the compaction is still running");
        pi.config.compaction_inactivity=std::time::Duration::from_millis(100);
        let r=tokio::time::timeout(std::time::Duration::from_secs(5),Harness::next(&mut pi)).await.unwrap();
        assert!(r.is_err(),"a compaction silent past its own limit still counts as stuck");
        std::fs::remove_file(&hold).unwrap();
        pi.shutdown().await.unwrap();
    }
    /// Night audit H8: Agent off waited 5 minutes for a compaction the harness let run 15, so Off cut slow summaries
    /// the user had paid for. Both read one limit.
    #[test]
    fn agent_off_waits_as_long_as_the_harness_lets_a_compaction_run() {
        let d=tempfile::tempdir().unwrap();
        let fixture=PiConfig::fixture(PathBuf::from("pi"),d.path().join("session"),d.path().into());
        assert_eq!(fixture.compaction_inactivity,crate::delivery::COMPACTION_WAIT);
        assert!(crate::delivery::COMPACTION_WAIT>=std::time::Duration::from_secs(900));
    }
    #[tokio::test]
    async fn repeated_compaction_frames_and_wrong_receipts_do_not_refresh_deadline() {
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        pi.config.inactivity=std::time::Duration::from_millis(60);
        pi.compact().await.unwrap();
        pi.ingest(json!({"type":"compaction_start"})).unwrap();let progress=pi.last_progress;
        for _ in 0..4 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            pi.ingest(json!({"type":"compaction_start"})).unwrap();
        }
        let refreshed=pi.last_progress!=progress;
        pi.ingest(json!({"type":"response","command":"compact","id":"maintenance-other","success":true})).unwrap();
        let falsely_settled=!pi.maintenance;
        pi.shutdown().await.unwrap();assert!(!refreshed,"duplicate frames refresh deadline");assert!(!falsely_settled);
    }
    /// "When the agent is compacting I should not be able to interrupt you". His Stop during
    /// Heimdall's idle compaction sends Pi no abort (that would throw the billed summary away); during a turn's own
    /// compaction it waits, and the turn stops as the compaction ends.
    #[tokio::test]
    async fn stop_never_aborts_a_compaction_and_stops_the_turn_once_it_ends() {
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        let aborts=|pi:&mut Pi|{let mut n=0;while let Some(v)=pi.process.try_next().unwrap(){if v["type"]=="mock_control"&&v["command"]=="abort"{n+=1;}}n};
        pi.maintenance=true;pi.maintenance_optional=true;pi.compacting=true;
        pi.abort().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(aborts(&mut pi),0,"the idle compaction was aborted");
        assert!(pi.maintenance && !pi.stop_after_compaction,"no turn to stop afterwards");
        pi.maintenance=false;pi.maintenance_optional=false;pi.compacting=false;

        pi.wakes.push_back(Wake{id:"w1".into(),consumed:true,accepted:true});
        pi.compacting=true;pi.compaction_started=true;
        pi.abort().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(aborts(&mut pi),0,"the turn's compaction was aborted");
        assert!(pi.stop_after_compaction && !pi.wakes.is_empty(),"the turn runs on while it compacts");
        pi.compacting=false;
        let (mut aborted,mut stopped)=(false,false);
        while !stopped {
            match tokio::time::timeout(std::time::Duration::from_secs(5),Harness::next(&mut pi)).await.unwrap().unwrap().unwrap() {
                Event::Other(v) if v["type"]=="mock_control"&&v["command"]=="abort" => aborted=true,
                Event::Other(v) if v["type"]=="heimdall_aborted" => {assert_eq!(v["id"],"w1");stopped=true;}
                _ => {}
            }
        }
        assert!(aborted,"Pi was told to stop the turn");
        assert!(!pi.stop_after_compaction&&pi.wakes.is_empty(),"stopped as the compaction ended");
        pi.shutdown().await.unwrap();
    }
    /// Agent off lets crucial work finish, then stops. Pi's own stop watcher keeps it alive while
    /// a compaction runs and while a state-writing tool call is in flight, and kills it as soon as both end.
    #[tokio::test]
    async fn agent_off_kills_pi_only_after_its_compaction_and_state_writes_end() {
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        let gate=Arc::new(crate::delivery::DeliveryGate::new(true));pi.set_delivery_gate(gate.clone());
        let exited=|pi:&mut Pi|pi.process.child.try_wait().unwrap().is_some();
        pi.wakes.push_back(Wake{id:"w1".into(),consumed:true,accepted:true});
        pi.ingest(json!({"type":"compaction_start","reason":"threshold"})).unwrap();
        assert!(pi.compaction_running());
        let write=gate.crucial_write(&gate.permit().unwrap()).unwrap();
        gate.intent(crate::delivery::StopReason::Agent,false);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!exited(&mut pi),"Off cut a running compaction");
        pi.ingest(json!({"type":"compaction_end","reason":"threshold","aborted":false,"willRetry":false})).unwrap();
        assert!(!pi.compaction_running());
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!exited(&mut pi),"Off cut a state write");
        drop(write);
        let began=std::time::Instant::now();
        while !exited(&mut pi) {assert!(began.elapsed()<std::time::Duration::from_secs(3),"Pi still runs after the crucial work ended");tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
        pi.shutdown().await.unwrap();
        // Heimdall's own (idle) compaction is held from its compact request on, until Pi answers it.
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        let gate=Arc::new(crate::delivery::DeliveryGate::new(true));pi.set_delivery_gate(gate.clone());
        std::fs::write(d.path().join("hold-compaction"),b"hold").unwrap();
        pi.compact().await.unwrap();
        gate.intent(crate::delivery::StopReason::Agent,false);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!exited(&mut pi)&&pi.compaction_in_progress(),"Off cut Heimdall's own compaction");
        std::fs::remove_file(d.path().join("hold-compaction")).unwrap();
        let began=std::time::Instant::now();
        while pi.compaction_running() {assert!(began.elapsed()<std::time::Duration::from_secs(3));let _=tokio::time::timeout(std::time::Duration::from_millis(100),Harness::next(&mut pi)).await;}
        while !exited(&mut pi) {assert!(began.elapsed()<std::time::Duration::from_secs(3),"Pi still runs after its compaction");tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
        pi.shutdown().await.unwrap();
        // Nothing crucial: Off kills at once, as before.
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        let gate=Arc::new(crate::delivery::DeliveryGate::new(true));pi.set_delivery_gate(gate.clone());
        gate.intent(crate::delivery::StopReason::Agent,false);
        let began=std::time::Instant::now();
        while !exited(&mut pi) {assert!(began.elapsed()<std::time::Duration::from_secs(3),"Off did not stop Pi at once");tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
        pi.shutdown().await.unwrap();
    }
    /// The extension's cold-cache compaction runs inside a wake's prompt (Pi reports it as "manual"): the harness
    /// shows it as compacting, counts it as progress, and clears it when it ends. Without a wake it is not ours.
    #[tokio::test]
    async fn cold_cache_compaction_before_a_wake_shows_as_compacting() {
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        pi.ingest(json!({"type":"compaction_start","reason":"manual"})).unwrap();
        assert!(!pi.compacting(),"no wake: not Heimdall's compaction");
        pi.wakes.push_back(Wake{id:"w1".into(),consumed:false,accepted:false});
        let before=pi.last_progress;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        pi.ingest(json!({"type":"compaction_start","reason":"manual"})).unwrap();
        assert!(pi.compacting() && pi.last_progress!=before,"the wake's compaction shows and keeps the turn alive");
        pi.ingest(json!({"type":"compaction_end","reason":"manual","result":{"usage":{"input":1}}})).unwrap();
        assert!(!pi.compacting());
        pi.shutdown().await.unwrap();
    }
    /// Night review 21 Oct (item 5): the in-run history repair no longer writes to Pi's RPC output itself; Pi
    /// announces its entry, and the harness turns it into the one event the log, island and lab read.
    #[tokio::test]
    async fn a_repair_entry_pi_announces_is_one_repair_event() {
        let d=tempfile::tempdir().unwrap();let mut pi=Pi::launch_config(PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into())).unwrap();
        pi.pending.clear();
        let entry=json!({"type":"custom","id":"r1","parentId":"e","customType":"heimdall_history_repair","data":{"state":"restarted_from_summary","line":"The model refused the conversation; continuing from its summary."}});
        pi.ingest(json!({"type":"entry_appended","entry":entry})).unwrap();
        let events:Vec<_>=pi.pending.drain(..).collect();
        assert!(matches!(&events[0],Event::Other(v) if *v==json!({"type":"heimdall_history_repair","state":"restarted_from_summary","line":"The model refused the conversation; continuing from its summary."})),"{events:?}");
        assert!(matches!(&events[1],Event::Other(v) if v["type"]=="entry_appended"),"the entry itself still reaches the Agent window: {events:?}");
        assert_eq!(events.len(),2);
        pi.ingest(json!({"type":"entry_appended","entry":{"type":"custom","customType":"heimdall_receipt","data":{"id":"x","completed":false}}})).unwrap();
        assert_eq!(pi.pending.len(),1,"other entries are not repairs");
        pi.shutdown().await.unwrap();
    }
    #[test]
    fn large_final_entry_recovers_exact_leaf_cursor() {
        let d=tempfile::tempdir().unwrap();let p=d.path().join("session");
        std::fs::write(&p,format!("{}\n{}\n",json!({"id":"previous"}),json!({"id":"leaf","body":"x".repeat(2*1024*1024)}))).unwrap();
        assert_eq!(last_entry_id(&p).unwrap(),Some("leaf".into()));
    }
    #[tokio::test]
    async fn pi_crash_is_observed_even_when_a_tool_holds_both_output_pipes() {
        let d=tempfile::tempdir().unwrap();let executable=node_script(d.path());
        std::fs::write(d.path().join("fixture.cjs"),r#"const cp=require('node:child_process');const fs=require('node:fs');const c=cp.spawn('/bin/sleep',['60'],{detached:true,stdio:['ignore',1,2]});fs.writeFileSync('tool.pid',String(c.pid));setInterval(()=>{},1000);"#).unwrap();
        let mut pi=Pi::launch_config(PiConfig::fixture(executable,d.path().join("session"),d.path().into())).unwrap();
        let tool=wait_file(&d.path().join("tool.pid")).await;
        pi.process.pi_identity.kill();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(3),pi.next()).await.unwrap().unwrap().is_none());
        pi.shutdown().await.unwrap();gone(tool).await;
    }
    #[tokio::test]
    async fn failed_guardian_identity_commit_never_releases_command_execution() {
        let d=tempfile::tempdir().unwrap();let registry=d.path().join("registry");
        std::fs::create_dir_all(registry.join("generation.owner")).unwrap();
        let owner=crate::process_owner::Owner::new();
        let result=Process::spawn_env(std::path::Path::new("/bin/sh"),&["-c".into(),"touch command-executed; sleep 60".into()],d.path(),&[(crate::process_owner::TOKEN.into(),owner.token),("HEIMDALL_TOOL_GROUPS".into(),registry.to_string_lossy().into_owned())]);
        assert!(result.is_err());assert!(!d.path().join("command-executed").exists());
    }
    #[tokio::test]
    async fn installed_pi_child_has_no_provider_key_or_mcp_token_in_env_or_argv() {
        let _lab = crate::provider::LAB_ITEMS_TEST.lock().await;
        use crate::provider::{ApiModel, HAIKU, OPENCODE_GO, PROVIDERS};
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        const KEY: &str = "sk-fake-harness-0123456789abcdefghij";
        const MCP: &str = "mcp-fake-harness-0123456789abcdefghij";
        for p in PROVIDERS {
            if std::env::var_os(p.key_env).is_some() { continue; }
            let d = tempfile::tempdir().unwrap();
            // A stand-in Pi: it records its argv and environment, then reads the MCP token from its inherited pipe.
            let script = d.path().join("pi.py");
            std::fs::write(&script, "#!/usr/bin/python3\nimport os,sys,time\nopen('argv.txt','w').write(' '.join(sys.argv))\nopen('env.txt','w').write('\\n'.join(f'{k}={v}' for k,v in os.environ.items()))\nopen('mcp.tmp','w').write(os.read(int(os.environ['HEIMDALL_MCP_TOKEN_FD']),4096).decode())\nos.rename('mcp.tmp','mcp.txt')\ntime.sleep(60)\n").unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
            crate::provider::save_key(p, KEY, None).unwrap();
            let mut config = PiConfig::fixture(script, d.path().join("session"), d.path().into());
            // [providers] Each provider runs a model of its own (a custom provider's with its endpoint).
            config.api_model = Some(if p.id == OPENCODE_GO { ApiModel::haiku() } else {
                ApiModel { provider: p.id.into(), base_url: p.custom_url.then(|| "http://127.0.0.1:9/v1".into()), ..ApiModel::anthropic(HAIKU).unwrap() }
            });
            config.mcp = Some(("http://127.0.0.1:9/mcp".into(), MCP.into()));
            let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(d.path().join("pi-owner.lock")).unwrap();
            // The installed app's handoff (`installed` true): the key is read by Pi's key command, never inherited.
            let process = config.spawn_with(lock.as_raw_fd(), None, true).unwrap();
            let start = std::time::Instant::now();
            while !d.path().join("mcp.txt").exists() { assert!(start.elapsed() < std::time::Duration::from_secs(10), "{}: the stand-in Pi never started", p.name); tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
            let argv = std::fs::read_to_string(d.path().join("argv.txt")).unwrap();
            let env = std::fs::read_to_string(d.path().join("env.txt")).unwrap();
            assert!(!argv.contains(KEY) && !env.contains(KEY), "{}: the key reached the child", p.name);
            assert!(!argv.contains(MCP) && !env.contains(MCP), "{}: the MCP token reached the child", p.name);
            assert!(!env.contains(p.key_env), "{}: the key variable is set", p.name);
            assert!(env.contains("HEIMDALL_MCP_TOKEN_FD=") && !env.contains("HEIMDALL_MCP_TOKEN="));
            assert_eq!(std::fs::read_to_string(d.path().join("mcp.txt")).unwrap(), MCP, "the pipe carries the MCP token");
            drop(process);
            crate::provider::forget_key(p).unwrap();
        }
    }
    /// The owned launch keeps Pi's MCP support on: no `--no-mcp`, so the user's mcp.json is read (docs/connectors.md).
    #[tokio::test]
    async fn owned_launch_leaves_pi_mcp_support_on() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        // A stand-in Pi: it records its argv, then exits.
        let script = d.path().join("pi.py");
        // Written whole, then renamed: the test waits for argv.txt and must never read it half written.
        std::fs::write(&script, "#!/usr/bin/python3\nimport os,sys\nopen('argv.tmp','w').write(' '.join(sys.argv))\nos.rename('argv.tmp','argv.txt')\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = PiConfig::fixture(script, d.path().join("session.jsonl"), d.path().into());
        // The owned launch's argv: the bundled CLI behind its preload (the runtime root only has to exist in a real app).
        config.cli = Some(d.path().join("runtime/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"));
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(d.path().join("pi-owner.lock")).unwrap();
        let _process = config.spawn_with(lock.as_raw_fd(), None, true).unwrap();
        let start = std::time::Instant::now();
        while !d.path().join("argv.txt").exists() { assert!(start.elapsed() < std::time::Duration::from_secs(10), "the stand-in Pi never started"); tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        let argv = std::fs::read_to_string(d.path().join("argv.txt")).unwrap();
        assert!(argv.contains("--mode rpc"), "{argv}");
        assert!(!argv.split(' ').any(|a| a == "--no-mcp"), "the launch turns Pi's MCP support off: {argv}");
    }
    /// Explicit pinned-runtime lab: the real Pi, launched the way Heimdall launches it (its extension, Heimdall's endpoint
    /// and the config's servers; no prompt, no model request). One session starts the stdio server that mcp.json names,
    /// and Heimdall's extension lists its own tools from its endpoint, stood in by the tiny server with `--http`.
    /// `HEIMDALL_PI_MCP_RUNTIME` is a runtime root with `node/`, `heimdall.ts` and `node_modules/@earendil-works/pi-coding-agent`
    /// (a built checkout's `target/agent-runtime/current`).
    #[tokio::test]
    #[ignore = "explicit pinned runtime lab: HEIMDALL_PI_MCP_RUNTIME"]
    async fn real_pi_starts_the_mcp_server_that_mcp_json_names_beside_heimdalls_own() {
        let runtime = PathBuf::from(std::env::var("HEIMDALL_PI_MCP_RUNTIME").unwrap());
        let d = tempfile::tempdir().unwrap();
        let node = runtime.join("node/bin/node");
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny-mcp.mjs");
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let http_log = d.path().join("http.log");
        let mut endpoint = std::process::Command::new(&node).arg(&server).arg("--http").arg(port.to_string()).env("TINY_MCP_HTTP_LOG", &http_log).spawn().unwrap();
        let start = std::time::Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(start.elapsed() < std::time::Duration::from_secs(10), "the stand-in endpoint never listened");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let mut config = PiConfig::fixture(node.clone(), d.path().join("session.jsonl"), d.path().into());
        config.cli = Some(runtime.join("node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"));
        config.extension = Some(runtime.join("heimdall.ts"));
        config.mcp = Some((format!("http://127.0.0.1:{port}/mcp"), "launch-token".into()));
        config.agent_dir = crate::runtime::configure(d.path(), 102400).unwrap();
        let marker = d.path().join("tools-list.txt");
        std::fs::write(
            config.agent_dir.join("mcp.json"),
            json!({"mcpServers": {"tiny": {"command": node, "args": [server], "env": {"TINY_MCP_MARKER": marker}, "exposure": "direct"}}}).to_string(),
        )
        .unwrap();
        let _pi = Pi::launch_config(config.clone()).unwrap();
        let listed = |log: &std::path::Path| std::fs::read_to_string(log).is_ok_and(|text| text.contains("tools/list"));
        let start = std::time::Instant::now();
        while !(marker.exists() && listed(&http_log)) && start.elapsed() < std::time::Duration::from_secs(90) {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let stdio_listed = marker.exists();
        let heimdall_listed = listed(&http_log);
        let _ = endpoint.kill();
        let _ = endpoint.wait();
        assert!(stdio_listed, "Pi never connected the server named in mcp.json");
        assert!(heimdall_listed, "Heimdall's extension never listed its own tools from its endpoint");
    }
    /// The prompt's connectors placeholder becomes the bundled guide's path in the app and the checkout's copy otherwise.
    #[test]
    fn connectors_guide_path_is_the_bundled_copy_in_the_app_and_the_checkout_otherwise() {
        let app = std::path::Path::new("/Applications/Heimdall.app/Contents/MacOS/Heimdall");
        let bundled = std::path::Path::new("/Applications/Heimdall.app/Contents/Resources/docs/connectors.md");
        assert_eq!(super::connectors_doc(app), bundled);
        let dev = super::connectors_doc(std::path::Path::new("/tmp/checkout/target/debug/heimdall"));
        assert!(dev.ends_with("docs/connectors.md") && dev.is_file(), "{}", dev.display());
        let filled = super::with_connectors_doc(PROMPT, bundled);
        assert!(!filled.contains(super::CONNECTORS_PLACEHOLDER));
        assert!(filled.contains("read the guide at `/Applications/Heimdall.app/Contents/Resources/docs/connectors.md`"));
    }
    #[tokio::test]
    async fn signed_main_startup_allowance_exceeds_old_helper_deadline() {
        let d=tempfile::tempdir().unwrap();let groups=d.path().join("groups");
        crate::runtime::private_dir(&groups).unwrap();let owner=crate::process_owner::Owner::new();
        let mut process=Process::spawn_env(std::path::Path::new("/usr/bin/python3"),
            &["-c".into(),"import os,time;open('executed.pid','w').write(str(os.getpid()));time.sleep(60)".into()],d.path(),&[
                (crate::process_owner::TOKEN.into(),owner.token),
                ("HEIMDALL_TOOL_GROUPS".into(),groups.to_string_lossy().into_owned()),
                ("HEIMDALL_TEST_GUARDIAN_DELAY_MS".into(),"3200".into())]).unwrap();
        let pid=wait_file(&d.path().join("executed.pid")).await;
        process.shutdown().await.unwrap();gone(pid).await;
    }

    #[tokio::test]
    async fn responsibility_failures_admit_with_persisted_limited_ownership() {
        for fault in ["missing", "attribute", "spawn"] {
            let d=tempfile::tempdir().unwrap();let groups=d.path().join("groups");
            crate::runtime::private_dir(&groups).unwrap();let owner=crate::process_owner::Owner::new();
            // These Python descendants remain inspectable by KERN_PROCARGS2 even
            // when protected Apple binaries would hide their inherited token.
            let fixture=d.path().join("escape.py");
            std::fs::write(&fixture,r#"import os,time
if os.fork()==0:
 os.setsid()
 if os.fork()!=0: os._exit(0)
 open('escaped.pid','w').write(str(os.getpid()))
 time.sleep(60)
else:
 open('executed.pid','w').write(str(os.getpid()))
 time.sleep(60)
"#).unwrap();
            let mut process=Process::spawn_env(std::path::Path::new("/usr/bin/python3"),
                &[fixture.to_string_lossy().into_owned()],d.path(),&[
                    (crate::process_owner::TOKEN.into(),owner.token),
                    ("HEIMDALL_TOOL_GROUPS".into(),groups.to_string_lossy().into_owned()),
                    ("HEIMDALL_TEST_RESPONSIBILITY_FAILURE".into(),fault.into())]).unwrap();
            let pid=wait_file(&d.path().join("executed.pid")).await;
            let escaped=wait_file(&d.path().join("escaped.pid")).await;
            assert!(process.owner.cleanup_limited, "{fault} did not reduce coverage");
            let receipt:Value=serde_json::from_slice(&std::fs::read(groups.join("generation.owner")).unwrap()).unwrap();
            assert_eq!(receipt["cleanup_limited"],true);
            process.shutdown().await.unwrap();gone(pid).await;gone(escaped).await;
        }
    }

    #[test]
    fn stale_birth_identity_never_signals_reused_pid_or_group() {
        use std::os::unix::process::CommandExt;
        let mut unrelated=std::process::Command::new("/bin/sleep").arg("60").process_group(0).spawn().unwrap();
        let mut identity=crate::process_owner::Identity::read(unrelated.id() as i32).unwrap();
        identity.invalidate_for_test();identity.kill();
        assert!(unrelated.try_wait().unwrap().is_none());unrelated.kill().unwrap();unrelated.wait().unwrap();
    }

    #[tokio::test]
    async fn dropping_retired_pi_after_launching_replacement_cannot_kill_replacement_tools() {
        let d=tempfile::tempdir().unwrap();let config=PiConfig::fixture(node_script(d.path()),d.path().join("session"),d.path().into());
        let mut old=Pi::launch_config(config.clone()).unwrap();wait_file(&d.path().join("sleep.pid")).await;old.shutdown().await.unwrap();
        std::fs::remove_file(d.path().join("tool.pid")).unwrap();std::fs::remove_file(d.path().join("sleep.pid")).unwrap();
        let mut replacement=Pi::launch_config(config).unwrap();let tool=wait_file(&d.path().join("tool.pid")).await;let sleep=wait_file(&d.path().join("sleep.pid")).await;
        drop(old);tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let alive=unsafe{libc::kill(tool,0)}==0 && unsafe{libc::kill(sleep,0)}==0;
        replacement.shutdown().await.unwrap();assert!(alive,"old Pi Drop killed replacement tools");
    }
    #[tokio::test]
    async fn unexpected_guardian_death_blocks_replacement_without_retirement_receipt() {
        let d=tempfile::tempdir().unwrap();
        let config=PiConfig::fixture(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mock-adapter.py"),d.path().join("session"),d.path().into());
        let mut pi=Pi::launch_config(config.clone()).unwrap();pi.state().await.unwrap();
        let root=pi.process.owner.root.unwrap();root.kill();pi.process.child.wait().await.unwrap();
        assert!(pi.shutdown().await.unwrap_err().to_string().contains("guardian died before retirement"));
        drop(pi);
        assert!(Pi::launch_config(config).err().unwrap().to_string().contains("retire previous tool generation"));
    }
    /// Exact pinned Pi's bash executor, invoked over RPC without a provider request or GPU.
    #[tokio::test]
    #[ignore = "explicit pinned runtime lab smoke"]
    async fn real_pi_120_bash_groups_retire_on_off_and_sigkill() {
        let runtime=PathBuf::from(std::env::var("HEIMDALL_PI_GROUP_SMOKE_RUNTIME").unwrap());
        let evidence=PathBuf::from(std::env::var("HEIMDALL_PI_GROUP_SMOKE_EVIDENCE").unwrap());
        let start=std::time::Instant::now();let mut results=vec![];
        for off in [true,false] {
            let d=tempfile::tempdir().unwrap();let mut config=PiConfig::fixture(runtime.join("node/bin/node"),d.path().join("session.jsonl"),d.path().into());
            config.cli=Some(runtime.join("node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js"));
            config.agent_dir=crate::runtime::configure(d.path(),102400).unwrap();
            std::fs::write(d.path().join("escape.py"),ESCAPING_CHAIN).unwrap();
            let mut unrelated=std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
            let unrelated_id=crate::process_owner::Identity::read(unrelated.id() as i32).unwrap();
            let mut pi=Pi::launch_config(config.clone()).unwrap();let gate=Arc::new(crate::delivery::DeliveryGate::new(true));pi.set_delivery_gate(gate.clone());
            pi.state().await.unwrap();
            // Pi's own bash executor (RPC `bash`): the bundled code path a model's bash tool uses.
            pi.write(json!({"type":"bash","id":"owned-bash","command":"echo $$ > tool.pid; /usr/bin/python3 escape.py & echo $! > sleep.pid; wait","excludeFromContext":true}),None).await.unwrap();
            let tool=wait_file(&d.path().join("tool.pid")).await;let descendant=wait_file(&d.path().join("sleep.pid")).await;let pid=pi.pid().unwrap();
            let detached=wait_file(&d.path().join("setsid.pid")).await;
            let double=wait_file(&d.path().join("double.pid")).await;
            let guardian=pi.process.child.id().unwrap();
            let mut path=vec![0u8;4096];
            let n=unsafe{libc::proc_pidpath(guardian as i32,path.as_mut_ptr() as *mut _,path.len() as u32)};
            assert!(n>0);let guardian_path=std::ffi::CStr::from_bytes_until_nul(&path).unwrap().to_str().unwrap().to_owned();
            let expected=std::env::var("HEIMDALL_TEST_GUARDIAN_EXECUTABLE").expect("live smoke requires the real Heimdall main executable");
            assert_eq!(guardian_path,expected);assert!(!pi.cleanup_limited(),"live smoke did not exercise responsibility ownership");
            assert_ne!(unsafe{libc::getpgid(tool)},pid as i32);
            assert_ne!(unsafe{libc::getpgid(detached)},unsafe{libc::getpgid(tool)});
            if off {gate.store(false,std::sync::atomic::Ordering::Release);}else{unsafe{libc::kill(pid as i32,libc::SIGKILL);};loop{if pi.next().await.unwrap().is_none(){break;}}}
            pi.shutdown().await.unwrap();gone(tool).await;gone(descendant).await;gone(detached).await;gone(double).await;gone(guardian as i32).await;drop(pi);
            assert_eq!(crate::process_owner::Identity::read(unrelated.id() as i32),Some(unrelated_id));
            let unrelated_pid=unrelated.id();unrelated.kill().unwrap();unrelated.wait().unwrap();
            let mut replacement=Pi::launch_config(config).unwrap();replacement.state().await.unwrap();let replacement_pid=replacement.pid().unwrap();
            let idle_guardian=replacement.process.child.id().unwrap();
            fn sample(pid:u32)->(u64,f64) {
                let output=std::process::Command::new("/bin/ps").args(["-p",&pid.to_string(),"-o","rss=,time="]).output().unwrap();
                let text=String::from_utf8(output.stdout).unwrap();let mut fields=text.split_whitespace();
                let rss=fields.next().unwrap().parse().unwrap();let time=fields.next().unwrap();
                let (minutes,seconds)=time.split_once(':').unwrap();
                (rss,minutes.parse::<f64>().unwrap()*60.0+seconds.parse::<f64>().unwrap())
            }
            let (_,before)=sample(idle_guardian);let idle_start=std::time::Instant::now();
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            let (rss,after)=sample(idle_guardian);let interval=idle_start.elapsed().as_secs_f64();
            replacement.shutdown().await.unwrap();
            results.push(json!({"stop":if off{"Agent Off"}else{"Pi SIGKILL"},"pi_pid":pid,"bash_group":tool,"python_pid":descendant,"setsid_sleep_pid":detached,"double_fork_sleep_pid":double,"guardian_pid":guardian,"guardian_executable":guardian_path,"cleanup_limited":false,"unrelated_sleep_pid":unrelated_pid,"unrelated_untouched":true,"survivors_before_replacement":0,"idle_guardian_rss_kib":rss,"idle_guardian_cpu_percent_one_core":100.0*(after-before)/interval,"idle_sample_seconds":interval,"replacement_pi":replacement_pid,"bash_and_sleep_gone_before_replacement":true}));
        }
        std::fs::write(evidence,serde_json::to_vec_pretty(&json!({"pi_version":"1.2.0","real_pi_tool":"Pi RPC bash: the bundled bash executor","provider_requests":0,"gpu_runs":0,"cases":results,"elapsed_seconds":start.elapsed().as_secs_f64(),"date_unix_ms":crate::now_ms()})).unwrap()).unwrap();
    }
}

/// A settled assistant answer is separate from per-delivery completion and MCP
/// speech. Keeping it singular prevents merged/steered wakes duplicating a turn.
/// The reply path (`crate::reply`): the text is what the user hears or reads, and `stop_reason` tells a finished reply
/// from an aborted or failed one. Every settled reply reports, also with empty text, so the turn's reply state closes.
fn final_answer_event(message: &Value) -> Option<Event> {
    if message["role"] != "assistant" || !matches!(message["stopReason"].as_str(),Some("stop"|"length"|"error"|"aborted")) {return None;}
    let text = message["content"].as_array()?.iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str()).collect::<Vec<_>>().join("\n");
    Some(Event::Other(json!({"type":"heimdall_final_answer","text":text,"stop_reason":message["stopReason"]})))
}

#[cfg(test)]
mod final_answer_tests {
    use super::*;
    #[test]
    fn settled_final_text_excludes_thinking_tools_and_diagnostic_errors() {
        let mut message=json!({"role":"assistant","stopReason":"stop","content":[
            {"type":"thinking","thinking":"private","text":"also private"},
            {"type":"toolCall","name":"speak_back","text":"tool narration"},
            {"type":"text","text":"The file is ready."},
            {"type":"text","text":"You can open it."}]});
        assert_eq!(final_answer_event(&message),Some(Event::Other(json!({"type":"heimdall_final_answer","text":"The file is ready.\nYou can open it.","stop_reason":"stop"}))));
        message["stopReason"]=json!("toolUse");
        assert!(final_answer_event(&message).is_none());
        // A reply that ended without text (silent, or tools only) still closes the turn's reply state.
        let silent=json!({"role":"assistant","stopReason":"stop","content":[{"type":"toolCall","name":"speak"}]});
        assert_eq!(final_answer_event(&silent),Some(Event::Other(json!({"type":"heimdall_final_answer","text":"","stop_reason":"stop"}))));
        for reason in ["length","aborted","error"] {
            message["stopReason"]=json!(reason);
            message["errorMessage"]=json!("private transport failure");
            let Some(Event::Other(event))=final_answer_event(&message) else {panic!("settled output text was lost");};
            assert_eq!(event["text"],"The file is ready.\nYou can open it.");
            assert_eq!(event["stop_reason"],reason,"the app says only finished replies");
        }
    }
}

#[cfg(test)]
mod prompt_source_tests {
    #[test]
    fn installed_app_uses_the_compiled_prompt_never_the_checkout_file() {
        let d = tempfile::tempdir().unwrap();
        let checkout = d.path().join("heimdall.md");
        std::fs::write(&checkout, "BRANCH PROMPT").unwrap();
        let app = std::path::Path::new("/Users/x/Applications/Heimdall.app/Contents/MacOS/Heimdall");
        assert_eq!(super::prompt_text(&checkout, app).unwrap(), super::PROMPT);
        let dev = d.path().join("target/debug/heimdall");
        assert_eq!(super::prompt_text(&checkout, &dev).unwrap(), "BRANCH PROMPT");
        assert_eq!(super::prompt_text(&d.path().join("absent.md"), &dev).unwrap(), super::PROMPT);
    }
    #[test]
    fn the_prompt_names_the_real_tool_log() {
        let prompt = super::PROMPT;
        // Editions (docs/editions.md): each build carries its own prompt, compaction text and Pi working folder.
        let public = cfg!(feature = "public");
        assert_eq!(prompt, if public { include_str!("../prompt/public/heimdall.md") } else { include_str!("../prompt/heimdall.md") });
        assert_eq!(super::COMPACTION_INSTRUCTIONS, if public { include_str!("../prompt/public/compaction.md") } else { include_str!("../prompt/compaction.md") });
        let home = heimdall_home::home_dir();
        assert_eq!(super::working_dir(), if home.is_dir() { home } else { std::path::PathBuf::from(std::env::var_os("HOME").unwrap()) });
        let data = std::path::Path::new("/Users/x/Library/Application Support/Heimdall");
        let log = crate::tool_log::path(data);
        let text = super::with_tool_log(prompt, Some(&log));
        assert!(!text.contains(crate::tool_log::PROMPT_PLACEHOLDER));
        assert!(text.contains("/Users/x/Library/Application Support/Heimdall/logs/tools.log`, newest last"), "{text}");
        assert!(text.contains("/Users/x/Library/Application Support/Heimdall/logs/tools.log.1`"));
        // The owned config logs into its data folder.
        let fixture = super::PiConfig::fixture("/bin/false".into(), "/tmp/s/session.jsonl".into(), "/tmp".into());
        assert_eq!(fixture.tool_log, None);
        assert!(!super::with_tool_log(prompt, None).contains(crate::tool_log::PROMPT_PLACEHOLDER));
    }
    /// Bug note 2026-10-27T03-22: the prompt named `docs/emergency.md` in a checkout on another branch. It names the
    /// running build's copy, and no checkout `docs/` path is fixed in the prompt.
    #[test]
    fn the_prompt_names_this_builds_emergency_protocol() {
        assert!(!super::PROMPT.contains("/Projects/Heimdall/docs/"), "a fixed checkout docs path");
        if !cfg!(feature = "public") { assert!(super::PROMPT.contains(super::EMERGENCY_PLACEHOLDER)); }
        let app = std::path::Path::new("/Applications/Heimdall.app/Contents/MacOS/heimdall");
        assert_eq!(super::emergency_doc(app), std::path::Path::new("/Applications/Heimdall.app/Contents/Resources/emergency.md"));
        assert!(super::emergency_doc(std::path::Path::new("/tmp/target/debug/heimdall")).is_file(), "the checkout's copy in development");
    }
    /// The prompt points at Heimdall's docs. It names the bundle's copy (this build's) when the bundle
    /// holds the index, and the checkout's `docs/` otherwise.
    #[test]
    fn the_prompt_names_this_builds_docs() {
        // The personal prompt is not in the public source export (there it is a copy of the public one).
        let personal_prompt = super::PROMPT != include_str!("../prompt/public/heimdall.md");
        if !cfg!(feature = "public") && personal_prompt { assert!(super::PROMPT.contains(&format!("`{}`", super::DOCS_PLACEHOLDER))); }
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("Heimdall.app/Contents/MacOS/Heimdall");
        let checkout = super::docs_dir(&exe);
        assert!(checkout.join("README.md").is_file() && checkout.join("emergency.md").is_file(), "{}", checkout.display());
        let bundled = d.path().join("Heimdall.app/Contents/Resources/docs");
        std::fs::create_dir_all(&bundled).unwrap();
        assert_eq!(super::docs_dir(&exe), checkout, "a bundle without the index falls back to the checkout");
        std::fs::write(bundled.join("README.md"), "# Docs\n").unwrap();
        assert_eq!(super::docs_dir(&exe), bundled);
    }
}
