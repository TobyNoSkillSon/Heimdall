//! Vella's borrowed transcription (docs/vella.md, Channel 2): a Unix socket in the data directory that decodes
//! another app's finished dictation on the Parakeet Ultra the listen worker already holds, and the
//! `dev.heimdall.asr` announcement that tells Vella the socket is there.
//!
//! The socket's accept and reads run on their own threads; the live loop only calls `VellaAsr::sync`, which takes a
//! lock and compares a pointer. Decoding happens on the listen worker's thread through
//! `ListenWorker::transcribe_external`, one 15-s window per scheduling turn between live drains. That audio never
//! reaches VAD, Smart Turn, the archive, the utterance pipeline, the decider or the agent.
//!
//! Self-contained (listen, serde_json, libc, objc2 only) so the lab server includes it by path:
//! `src/bin/heimdall-vella-asr-lab.rs`.
use anyhow::{bail, ensure, Context, Result};
use listen::{ExternalRefusal, ExternalReply, ListenWorker, EXTERNAL_MAX_SAMPLES};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::unix::{
        fs::{FileTypeExt, MetadataExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// The one model id this socket serves; Vella names its selection the same way.
pub const MODEL: &str = "parakeet-v3-ultra";
pub const SOCKET_NAME: &str = "asr.sock";
/// The editions' app IDs (crates/home on the combined branch; `scripts/bundle.sh`).
pub const PERSONAL_ID: &str = "dev.heimdall.app";
pub const PUBLIC_ID: &str = "com.tobynoskillson.heimdall";
/// This build's app ID, announced so Vella can match the socket's owner to its edition.
pub fn bundle_id(public: bool) -> &'static str {
    if public { PUBLIC_ID } else { PERSONAL_ID }
}
/// Where the socket lives: a fixed folder per edition, never the (movable) data folder and never shared between
/// editions (docs/vella.md). Personal: `~/Library/Application Support/Heimdall`, the profile's original place, which
/// still resolves after a home move through the link left there. Public: `~/Library/Application
/// Support/com.tobynoskillson.heimdall`, the pointer folder beside `home.json`. A lab or test run on its own `--data`
/// keeps the socket in that folder, so it never takes an installed app's socket.
pub fn socket_dir(data: &Path, default_data: Option<&Path>, user_home: Option<&Path>, public: bool) -> PathBuf {
    match (default_data, user_home) {
        (Some(default), Some(home)) if data == default => {
            home.join("Library/Application Support").join(if public { PUBLIC_ID } else { "Heimdall" })
        }
        _ => data.to_path_buf(),
    }
}
/// A header is one short JSON line.
const HEADER_LIMIT: u64 = 4096;
/// Connections handled at once; more get `busy` from the accept thread. Two hold the decode slots, the rest are
/// status checks or requests about to be refused.
const MAX_CONNECTIONS: usize = 4;
/// A single read may stall this long before the request is dropped (a stuck client never holds a thread for good).
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole header line arrives within this, however it trickles (a client sending a byte every few seconds would
/// otherwise hold a connection thread for hours, and four of them make every request busy).
const HEADER_DEADLINE: Duration = Duration::from_secs(2);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    Busy,
    Unavailable,
    TooLong,
    BadRequest,
    ModelMismatch,
}
impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::TooLong => "too_long",
            Self::BadRequest => "bad_request",
            Self::ModelMismatch => "model_mismatch",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Status,
    Transcribe { samples: usize },
}
/// One header line (with or without its newline). Order of checks: shape, model, format, length.
pub fn parse_header(line: &[u8]) -> Result<Request, ErrorCode> {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let v: serde_json::Value = serde_json::from_slice(line).map_err(|_| ErrorCode::BadRequest)?;
    let o = v.as_object().ok_or(ErrorCode::BadRequest)?;
    if o.get("v").and_then(|v| v.as_u64()) != Some(1) {
        return Err(ErrorCode::BadRequest);
    }
    match o.get("op").and_then(|v| v.as_str()) {
        Some("status") => Ok(Request::Status),
        Some("transcribe") => {
            match o.get("model").and_then(|v| v.as_str()) {
                Some(MODEL) => {}
                Some(_) => return Err(ErrorCode::ModelMismatch),
                None => return Err(ErrorCode::BadRequest),
            }
            if o.get("rate").and_then(|v| v.as_u64()) != Some(16_000) || o.get("format").and_then(|v| v.as_str()) != Some("f32le") {
                return Err(ErrorCode::BadRequest);
            }
            let samples = o.get("samples").and_then(|v| v.as_u64()).ok_or(ErrorCode::BadRequest)?;
            if samples > EXTERNAL_MAX_SAMPLES as u64 {
                return Err(ErrorCode::TooLong);
            }
            Ok(Request::Transcribe { samples: samples as usize })
        }
        _ => Err(ErrorCode::BadRequest),
    }
}
pub fn error_line(code: ErrorCode) -> String {
    format!("{}\n", serde_json::json!({"v": 1, "ok": false, "error": code.as_str()}))
}
fn ok_line(text: &str, queue_ms: f64, decode_ms: f64, audio_ms: u64, model_ms: f64, windows: usize) -> String {
    format!(
        "{}\n",
        serde_json::json!({"v": 1, "ok": true, "text": text, "queue_ms": queue_ms.round() as u64,
            "decode_ms": decode_ms.round() as u64, "audio_ms": audio_ms, "model_ms": model_ms.round() as u64, "windows": windows})
    )
}
fn status_line(full: bool) -> String {
    format!("{}\n", serde_json::json!({"v": 1, "ok": true, "state": "ready", "model": MODEL, "busy": full}))
}

/// What the socket decodes with: the live worker in the app, a fake in tests.
pub trait Decoder: Send + Sync {
    /// The model is loaded and accepting work.
    fn available(&self) -> bool;
    /// Both request slots are taken.
    fn full(&self) -> bool;
    fn submit(&self, samples: Vec<f32>) -> Result<ExternalReply, ExternalRefusal>;
}
/// The listen worker Heimdall holds right now, if any. Swapped by `VellaAsr::sync` when the worker starts or ends.
#[derive(Default)]
pub struct LiveModel(Mutex<Option<Arc<ListenWorker>>>);
impl LiveModel {
    fn current(&self) -> Option<Arc<ListenWorker>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn set(&self, worker: Option<Arc<ListenWorker>>) {
        let mut slot = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let same = match (&*slot, &worker) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            *slot = worker;
        }
    }
}
impl Decoder for LiveModel {
    fn available(&self) -> bool {
        self.current().is_some_and(|w| w.external_available())
    }
    fn full(&self) -> bool {
        self.current().is_some_and(|w| w.external_full())
    }
    fn submit(&self, samples: Vec<f32>) -> Result<ExternalReply, ExternalRefusal> {
        self.current().ok_or(ExternalRefusal::Unavailable)?.transcribe_external(samples)
    }
}

/// The socket server. Dropping it stops accepting and removes the socket file it created.
pub struct Server {
    path: PathBuf,
    inode: u64,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Server {
    /// Bind `path` with mode 0600 (set before `listen`, so nobody connects to a wider socket), removing a stale
    /// socket first. A socket another process still answers on is left alone and is an error.
    pub fn start(path: PathBuf, decoder: Arc<dyn Decoder>) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        clear_stale(&path)?;
        let listener = bind_private(&path)?;
        let inode = std::fs::symlink_metadata(&path)?.ino();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new().name("heimdall-vella-asr".into()).spawn(move || {
            let active = Arc::new(AtomicUsize::new(0));
            for conn in listener.incoming() {
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let Ok(mut stream) = conn else {
                    // A lasting accept error (out of descriptors) must not spin.
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                };
                if active.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
                    let _ = stream.write_all(error_line(ErrorCode::Busy).as_bytes());
                    continue;
                }
                active.fetch_add(1, Ordering::AcqRel);
                let (decoder, held) = (decoder.clone(), active.clone());
                let spawned = std::thread::Builder::new().name("heimdall-vella-asr-conn".into()).spawn(move || {
                    handle(stream, &*decoder);
                    held.fetch_sub(1, Ordering::AcqRel);
                });
                if spawned.is_err() {
                    active.fetch_sub(1, Ordering::AcqRel);
                }
            }
        })?;
        Ok(Self { path, inode, stop, thread: Some(thread) })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Only our own socket: a newer instance may have replaced it, or it was deleted.
        let ours = std::fs::symlink_metadata(&self.path).is_ok_and(|m| m.ino() == self.inode);
        // Wake the blocking accept; it sees the flag and ends. Wait for it only when the wake reached our listener:
        // otherwise the thread is left blocked (it serves nothing once the flag is set) rather than hanging Quit.
        let woke = ours && UnixStream::connect(&self.path).is_ok();
        if let Some(t) = self.thread.take() {
            if woke {
                let _ = t.join();
            }
        }
        if ours {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
fn clear_stale(path: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    };
    ensure!(meta.file_type().is_socket(), "{} exists and is not a socket; leaving it", path.display());
    if UnixStream::connect(path).is_ok() {
        bail!("another process answers on {}; not taking it over", path.display());
    }
    std::fs::remove_file(path).with_context(|| format!("remove stale {}", path.display()))
}
fn bind_private(path: &Path) -> Result<UnixListener> {
    use std::os::{fd::FromRawFd, unix::ffi::OsStrExt};
    let bytes = path.as_os_str().as_bytes();
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    ensure!(bytes.len() < addr.sun_path.len() && !bytes.contains(&0), "socket path too long: {}", path.display());
    for (d, s) in addr.sun_path.iter_mut().zip(bytes) {
        *d = *s as libc::c_char;
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        addr.sun_len = len as u8;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    ensure!(raw >= 0, "socket: {}", std::io::Error::last_os_error());
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
    use std::os::fd::AsRawFd;
    unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    ensure!(
        unsafe { libc::bind(fd.as_raw_fd(), &addr as *const _ as *const libc::sockaddr, len) } == 0,
        "bind {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    if let Err(e) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(e).with_context(|| format!("chmod {}", path.display()));
    }
    if unsafe { libc::listen(fd.as_raw_fd(), 16) } != 0 {
        let e = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(path);
        bail!("listen {}: {e}", path.display());
    }
    Ok(UnixListener::from(fd))
}
/// Answer, then read and discard what the client still sends (bounded), so a client that writes its whole request
/// before reading sees the answer instead of a broken pipe.
fn answer_and_drain(stream: &UnixStream, reader: &mut impl Read, line: &str) {
    let mut w = stream;
    let _ = w.write_all(line.as_bytes());
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = [0u8; 65536];
    let mut left: usize = EXTERNAL_MAX_SAMPLES * 4 + 65536;
    while left > 0 && Instant::now() < deadline {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => left = left.saturating_sub(n),
        }
    }
}
/// The header line, up to `HEADER_LIMIT` bytes and its newline, read within `deadline` in total: before each read the
/// timeout is the time left. Incomplete (no newline) on a deadline, a close or an error.
fn read_header(stream: &UnixStream, reader: &mut BufReader<&UnixStream>, deadline: Duration) -> Vec<u8> {
    let until = Instant::now() + deadline;
    let mut header = Vec::new();
    while header.len() < HEADER_LIMIT as usize {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left.min(READ_TIMEOUT))).is_err() {
            break;
        }
        let buf = match reader.fill_buf() {
            Ok(b) if b.is_empty() => break,
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let room = &buf[..buf.len().min(HEADER_LIMIT as usize - header.len())];
        if let Some(i) = room.iter().position(|b| *b == b'\n') {
            header.extend_from_slice(&room[..=i]);
            reader.consume(i + 1);
            break;
        }
        let n = room.len();
        header.extend_from_slice(room);
        reader.consume(n);
    }
    header
}
fn handle(stream: UnixStream, decoder: &dyn Decoder) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut reader = BufReader::with_capacity(1 << 16, &stream);
    let header = read_header(&stream, &mut reader, HEADER_DEADLINE);
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let complete = header.ends_with(b"\n");
    let request = if complete { parse_header(&header) } else { Err(ErrorCode::BadRequest) };
    let samples = match request {
        Err(code) => return answer_and_drain(&stream, &mut reader, &error_line(code)),
        Ok(Request::Status) => {
            let line = if decoder.available() { status_line(decoder.full()) } else { error_line(ErrorCode::Unavailable) };
            return answer_and_drain(&stream, &mut reader, &line);
        }
        Ok(Request::Transcribe { samples }) => samples,
    };
    // Refuse before reading audio we would not decode.
    if !decoder.available() {
        return answer_and_drain(&stream, &mut reader, &error_line(ErrorCode::Unavailable));
    }
    if decoder.full() {
        return answer_and_drain(&stream, &mut reader, &error_line(ErrorCode::Busy));
    }
    // The whole body arrives within 5 s plus its own length (a local client writes 10 min of audio in well under a
    // second), so a trickling client cannot hold a connection thread.
    let deadline = Instant::now() + Duration::from_secs(5) + Duration::from_millis(samples as u64 / 16);
    let mut bytes = vec![0u8; samples * 4];
    let mut filled = 0;
    while filled < bytes.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left.min(READ_TIMEOUT))).is_err() {
            break;
        }
        match reader.read(&mut bytes[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    if filled < bytes.len() {
        return answer_and_drain(&stream, &mut reader, &error_line(ErrorCode::BadRequest));
    }
    let audio: Vec<f32> = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    drop(bytes);
    let audio_ms = samples as u64 / 16;
    let line = match decoder.submit(audio) {
        Err(ExternalRefusal::Busy) => error_line(ErrorCode::Busy),
        Err(ExternalRefusal::Unavailable) => error_line(ErrorCode::Unavailable),
        Err(ExternalRefusal::Invalid(_)) => error_line(ErrorCode::BadRequest),
        // Bounded so a stuck worker never pins this thread; Vella has long given up by then (5 s + audio/10).
        Ok(reply) => match reply.recv_timeout(Duration::from_secs(60) + Duration::from_millis(audio_ms)) {
            Ok(Ok(t)) => ok_line(&t.transcript.text, t.queue_ms, t.decode_ms, audio_ms, t.model_ms, t.windows),
            Ok(Err(e)) => {
                eprintln!("Vella transcription failed: {e}");
                error_line(ErrorCode::Unavailable)
            }
            Err(_) => error_line(ErrorCode::Unavailable),
        },
    };
    answer_and_drain(&stream, &mut reader, &line);
}

/// Lab only (the standalone server and the opt-in model test): the listen worker as the app builds it, with the
/// same queue bounds and backends, and nothing else of Heimdall.
#[allow(dead_code)]
pub fn spawn_listen_worker(models: &Path, parakeet_backend: &str, turn_backend: &str) -> Result<ListenWorker> {
    let (models, parakeet_backend, turn_backend) = (models.to_path_buf(), parakeet_backend.to_string(), turn_backend.to_string());
    ListenWorker::spawn(listen::QueueConfig { chunks_per_source: 768, ..Default::default() }, move || {
        let asr = listen::asr::Parakeet::load_backend(&models.join("parakeet-ultra"), &parakeet_backend)?;
        listen::ListenEngine::load_with_turn_backend(&models, asr, Some(&turn_backend))
    })
}

/// `dev.heimdall.asr` on the distributed center, and the answer to Vella's `dev.vella.asr.query`.
#[cfg(target_os = "macos")]
mod announce {
    use objc2::{rc::Retained, runtime::{AnyObject, ProtocolObject}};
    use objc2_foundation::{NSDictionary, NSDistributedNotificationCenter, NSNotification, NSNumber, NSObjectProtocol, NSString};
    use std::{ptr::NonNull, sync::{Arc, Mutex}};
    pub const NAME: &str = "dev.heimdall.asr";
    pub const QUERY: &str = "dev.vella.asr.query";
    pub const OBJECT: &str = "dev.heimdall";
    fn post(ready: bool, socket: &str) {
        let sent = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0., |d| d.as_secs_f64());
        let keys = ["v", "state", "socket", "model", "pid", "sent", "app"].map(NSString::from_str);
        let as_any = |o: Retained<objc2_foundation::NSObject>| unsafe { Retained::cast_unchecked::<AnyObject>(o) };
        let values: Vec<Retained<AnyObject>> = vec![
            as_any(Retained::into_super(Retained::into_super(NSNumber::new_i64(1)))),
            as_any(Retained::into_super(NSString::from_str(if ready { "ready" } else { "gone" }))),
            as_any(Retained::into_super(NSString::from_str(socket))),
            as_any(Retained::into_super(NSString::from_str(super::MODEL))),
            as_any(Retained::into_super(Retained::into_super(NSNumber::new_i64(std::process::id() as i64)))),
            as_any(Retained::into_super(Retained::into_super(NSNumber::new_f64(sent)))),
            as_any(Retained::into_super(NSString::from_str(super::bundle_id(cfg!(feature = "public"))))),
        ];
        let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
        let info = NSDictionary::<NSString, AnyObject>::from_retained_objects(&key_refs, &values);
        let info: &NSDictionary = unsafe { &*(Retained::as_ptr(&info) as *const NSDictionary) };
        unsafe {
            NSDistributedNotificationCenter::defaultCenter().postNotificationName_object_userInfo_deliverImmediately(
                &NSString::from_str(NAME),
                Some(&NSString::from_str(OBJECT)),
                Some(info),
                true,
            );
        }
    }
    pub struct Announcer {
        state: Arc<Mutex<bool>>,
        socket: String,
        token: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    }
    impl Announcer {
        /// Starts as `gone` without posting; Vella's query is answered with the current state.
        pub fn new(socket: String) -> Self {
            let state = Arc::new(Mutex::new(false));
            let (seen, path) = (state.clone(), socket.clone());
            let block = block2::RcBlock::new(move |_: NonNull<NSNotification>| {
                let ready = *seen.lock().unwrap_or_else(|e| e.into_inner());
                post(ready, &path);
            });
            let token = unsafe {
                NSDistributedNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
                    Some(&NSString::from_str(QUERY)),
                    None,
                    None,
                    &block,
                )
            };
            Self { state, socket, token }
        }
        pub fn set(&self, ready: bool) {
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = ready;
            post(ready, &self.socket);
        }
    }
    impl Drop for Announcer {
        fn drop(&mut self) {
            unsafe {
                NSDistributedNotificationCenter::defaultCenter()
                    .removeObserver(&*(Retained::as_ptr(&self.token) as *const AnyObject));
            }
        }
    }
}
#[cfg(not(target_os = "macos"))]
mod announce {
    pub struct Announcer;
    impl Announcer {
        pub fn new(_: String) -> Self { Self }
        pub fn set(&self, _: bool) {}
    }
}

/// The socket, its announcement and the model it lends. Owned by the live loop; `sync` after anything that may start,
/// stop or ready the listen worker. Dropping it announces `gone`, then closes and removes the socket.
pub struct VellaAsr {
    model: Arc<LiveModel>,
    path: PathBuf,
    server: Option<Server>,
    /// When the socket failed to start (an older Heimdall still quitting holds it), the next try.
    retry_at: Option<Instant>,
    announcer: Option<announce::Announcer>,
    ready: bool,
}
/// A failed socket start is retried this often from `sync`.
const RETRY: Duration = Duration::from_secs(30);
impl VellaAsr {
    /// `data` is the app's data directory (`~/Library/Application Support/Heimdall` for the installed app); the
    /// socket is `asr.sock` in it. `announce` posts `dev.heimdall.asr`; off for tests.
    /// A socket that cannot start now (held by an older Heimdall still quitting) is an error for the caller to log;
    /// the service still runs and `sync` retries every 30 s.
    pub fn start(data: &Path, announce: bool) -> (Self, Option<anyhow::Error>) {
        let model = Arc::new(LiveModel::default());
        let path = data.join(SOCKET_NAME);
        let (server, error) = match Server::start(path.clone(), model.clone()) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(e)),
        };
        let announcer = announce.then(|| announce::Announcer::new(path.to_string_lossy().into_owned()));
        let retry_at = server.is_none().then(|| Instant::now() + RETRY);
        (Self { model, path, server, retry_at, announcer, ready: false }, error)
    }
    pub fn socket(&self) -> &Path {
        &self.path
    }
    /// Lend `worker` (the current listen worker, or none) and announce a change in readiness. Cheap: no inference,
    /// no IO unless readiness changed.
    pub fn sync(&mut self, worker: Option<Arc<ListenWorker>>) {
        self.model.set(worker);
        if self.retry_at.is_some_and(|at| Instant::now() >= at) {
            match Server::start(self.path.clone(), self.model.clone()) {
                Ok(s) => { self.server = Some(s); self.retry_at = None; }
                Err(_) => self.retry_at = Some(Instant::now() + RETRY),
            }
        }
        let ready = self.server.is_some() && self.model.available();
        if ready != self.ready {
            self.ready = ready;
            if let Some(a) = &self.announcer {
                a.set(ready);
            }
        }
    }
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.ready
    }
}
impl Drop for VellaAsr {
    fn drop(&mut self) {
        // Vella hears `gone` before the socket disappears, and stops borrowing.
        if let Some(a) = self.announcer.take() {
            a.set(false);
        }
        self.model.set(None);
        self.server.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use listen::{ExternalTranscript, Transcript};
    use std::sync::mpsc;

    #[test]
    fn each_edition_has_its_own_fixed_socket_folder_and_a_lab_folder_keeps_its_own() {
        let home = Path::new("/Users/someone");
        let moved = Path::new("/Volumes/Elsewhere/.heimdall");
        // Personal on its (possibly moved) default data: the original profile folder, where the installed Vella looks.
        assert_eq!(socket_dir(moved, Some(moved), Some(home), false), home.join("Library/Application Support/Heimdall"));
        // Public: its own pointer folder, never the personal edition's folder.
        let public = socket_dir(moved, Some(moved), Some(home), true);
        assert_eq!(public, home.join("Library/Application Support/com.tobynoskillson.heimdall"));
        assert_ne!(public, socket_dir(moved, Some(moved), Some(home), false));
        // Within the 104-byte sun_path limit for user names up to 31 characters; a longer one makes the socket's
        // start fail (logged, retried) and Vella uses its own model.
        let long = Path::new("/Users/abcdefghijklmnopqrstuvwxyz01234");
        assert!(socket_dir(moved, Some(moved), Some(long), true).join(SOCKET_NAME).as_os_str().len() < 104);
        // A lab or test run on its own --data never takes an installed app's socket, in either edition.
        for public in [false, true] {
            assert_eq!(socket_dir(Path::new("/tmp/lab"), Some(moved), Some(home), public), PathBuf::from("/tmp/lab"));
            assert_eq!(socket_dir(Path::new("/tmp/lab"), None, Some(home), public), PathBuf::from("/tmp/lab"));
        }
        assert_eq!((bundle_id(false), bundle_id(true)), (PERSONAL_ID, PUBLIC_ID));
    }

    fn request(path: &Path, header: &str, audio: &[f32]) -> serde_json::Value {
        let mut s = UnixStream::connect(path).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s.write_all(header.as_bytes()).unwrap();
        let bytes: Vec<u8> = audio.iter().flat_map(|v| v.to_le_bytes()).collect();
        // A refusal may close the read side early; the answer must still arrive.
        let _ = s.write_all(&bytes);
        let mut line = String::new();
        BufReader::new(&s).read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    /// Review 27 Oct: a client trickling its header a byte at a time cannot hold a connection thread past the header
    /// deadline (each read's own timeout never expires while bytes keep coming); a header that arrives in pieces
    /// within it is read whole, and what follows the newline stays for the body.
    #[test]
    fn a_trickled_header_ends_at_its_total_deadline() {
        let (server, mut client) = UnixStream::pair().unwrap();
        let trickle = std::thread::spawn(move || {
            for _ in 0..20 {
                if client.write_all(b"{").is_err() { break; }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let began = Instant::now();
        let mut reader = BufReader::new(&server);
        let header = read_header(&server, &mut reader, Duration::from_millis(300));
        let took = began.elapsed();
        assert!(!header.ends_with(b"\n") && !header.is_empty(), "incomplete: {header:?}");
        assert!(took < Duration::from_millis(600), "held for {took:?}");
        drop(reader);
        drop(server);
        trickle.join().unwrap();

        let (server, mut client) = UnixStream::pair().unwrap();
        let line = header_line_for_test();
        let pieces = std::thread::spawn(move || {
            let (a, b) = line.as_bytes().split_at(10);
            client.write_all(a).unwrap();
            std::thread::sleep(Duration::from_millis(60));
            client.write_all(b).unwrap();
            client.write_all(&[1, 2, 3, 4]).unwrap();
            client
        });
        let mut reader = BufReader::new(&server);
        let header = read_header(&server, &mut reader, Duration::from_millis(500));
        assert_eq!(header, header_line_for_test().into_bytes());
        let mut rest = [0u8; 4];
        reader.read_exact(&mut rest).unwrap();
        assert_eq!(rest, [1, 2, 3, 4], "the body's first bytes stay buffered");
        drop(pieces.join().unwrap());
    }
    fn header_line_for_test() -> String {
        header(4)
    }
    fn header(samples: usize) -> String {
        format!("{{\"v\":1,\"op\":\"transcribe\",\"model\":\"parakeet-v3-ultra\",\"rate\":16000,\"samples\":{samples},\"format\":\"f32le\"}}\n")
    }
    /// A decoder that holds every request until released, with a fixed number of slots.
    struct Held {
        available: AtomicBool,
        slots: Mutex<Vec<mpsc::Sender<Result<ExternalTranscript, String>>>>,
        submitted: AtomicUsize,
    }
    impl Held {
        fn new() -> Arc<Self> {
            Arc::new(Self { available: AtomicBool::new(true), slots: Mutex::new(vec![]), submitted: AtomicUsize::new(0) })
        }
        fn release(&self, text: &str) {
            for tx in self.slots.lock().unwrap().drain(..) {
                let _ = tx.send(Ok(ExternalTranscript { transcript: Transcript { text: text.into(), words: vec![] }, queue_ms: 1., decode_ms: 2., model_ms: 2., windows: 1 }));
            }
        }
    }
    impl Decoder for Held {
        fn available(&self) -> bool { self.available.load(Ordering::Acquire) }
        fn full(&self) -> bool { self.slots.lock().unwrap().len() >= 2 }
        fn submit(&self, samples: Vec<f32>) -> Result<ExternalReply, ExternalRefusal> {
            assert!(samples.iter().all(|v| *v == 0.25), "audio arrives intact");
            let mut slots = self.slots.lock().unwrap();
            if slots.len() >= 2 { return Err(ExternalRefusal::Busy); }
            let (tx, rx) = mpsc::channel();
            slots.push(tx);
            self.submitted.fetch_add(1, Ordering::AcqRel);
            Ok(rx)
        }
    }

    #[test]
    fn headers_parse_and_refuse_by_the_spec() {
        assert_eq!(parse_header(header(16000).as_bytes()), Ok(Request::Transcribe { samples: 16000 }));
        assert_eq!(parse_header(b"{\"v\":1,\"op\":\"status\"}\n"), Ok(Request::Status));
        assert_eq!(parse_header(header(EXTERNAL_MAX_SAMPLES).as_bytes()), Ok(Request::Transcribe { samples: 9_600_000 }));
        assert_eq!(parse_header(header(EXTERNAL_MAX_SAMPLES + 1).as_bytes()), Err(ErrorCode::TooLong));
        let other = header(10).replace("parakeet-v3-ultra", "whisper-large-v3");
        assert_eq!(parse_header(other.as_bytes()), Err(ErrorCode::ModelMismatch));
        for bad in [
            "not json\n".to_string(),
            "[1]\n".into(),
            "{\"v\":2,\"op\":\"status\"}\n".into(),
            "{\"v\":1,\"op\":\"reboot\"}\n".into(),
            header(10).replace("16000", "48000"),
            header(10).replace("f32le", "s16le"),
            header(10).replace(",\"samples\":10", ""),
            header(10).replace("10,", "-1,"),
            header(10).replace(",\"model\":\"parakeet-v3-ultra\"", ""),
        ] {
            assert_eq!(parse_header(bad.as_bytes()), Err(ErrorCode::BadRequest), "{bad}");
        }
        assert_eq!(error_line(ErrorCode::TooLong), "{\"error\":\"too_long\",\"ok\":false,\"v\":1}\n");
    }
    #[test]
    fn socket_is_private_replaces_a_stale_one_and_is_removed_at_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        // A stale socket from a crashed run: bound, never served.
        drop(UnixListener::bind(&path).unwrap());
        assert!(std::fs::symlink_metadata(&path).unwrap().file_type().is_socket());
        let server = Server::start(path.clone(), Held::new()).unwrap();
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.file_type().is_socket());
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // A live one is not taken over.
        assert!(Server::start(path.clone(), Held::new()).is_err());
        drop(server);
        assert!(!path.exists(), "socket removed at shutdown");
        // A socket deleted under a running server: dropping it neither hangs nor touches the path.
        let server = Server::start(path.clone(), Held::new()).unwrap();
        std::fs::remove_file(&path).unwrap();
        let at = Instant::now();
        drop(server);
        assert!(at.elapsed() < Duration::from_secs(1));
        // A regular file in its place is never deleted.
        std::fs::write(&path, b"keep").unwrap();
        assert!(Server::start(path.clone(), Held::new()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
    #[test]
    fn transcribe_status_and_every_refusal_over_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SOCKET_NAME);
        let decoder = Held::new();
        let _server = Server::start(path.clone(), decoder.clone()).unwrap();
        let status = request(&path, "{\"v\":1,\"op\":\"status\"}\n", &[]);
        assert_eq!((status["ok"].as_bool(), status["model"].as_str(), status["busy"].as_bool()), (Some(true), Some(MODEL), Some(false)));
        assert_eq!(request(&path, "garbage\n", &[])["error"], "bad_request");
        assert_eq!(request(&path, &header(EXTERNAL_MAX_SAMPLES + 1), &[0.25; 64])["error"], "too_long");
        assert_eq!(request(&path, &header(16).replace("parakeet-v3-ultra", "parakeet-v2"), &[0.25; 16])["error"], "model_mismatch");
        // Fewer samples than declared: the read fails and the request is bad, not decoded.
        {
            let mut s = UnixStream::connect(&path).unwrap();
            s.write_all(header(1000).as_bytes()).unwrap();
            s.write_all(&[0u8; 400]).unwrap();
            s.shutdown(std::net::Shutdown::Write).unwrap();
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).unwrap();
            assert!(line.contains("bad_request"), "{line}");
        }
        // Two requests take both slots and wait on the held decoder; a third is busy.
        let (p1, p2) = (path.clone(), path.clone());
        let a = std::thread::spawn(move || request(&p1, &header(1600), &[0.25; 1600]));
        let b = std::thread::spawn(move || request(&p2, &header(3200), &[0.25; 3200]));
        let start = Instant::now();
        while decoder.submitted.load(Ordering::Acquire) < 2 {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(request(&path, &header(1600), &[0.25; 1600])["error"], "busy");
        assert_eq!(request(&path, "{\"v\":1,\"op\":\"status\"}\n", &[])["busy"], true);
        decoder.release("hello there");
        for (reply, ms) in [(a.join().unwrap(), 100), (b.join().unwrap(), 200)] {
            assert_eq!(reply["ok"], true);
            assert_eq!(reply["text"], "hello there");
            assert_eq!(reply["audio_ms"], ms);
            assert!(reply["queue_ms"].is_u64() && reply["decode_ms"].is_u64());
        }
        decoder.available.store(false, Ordering::Release);
        assert_eq!(request(&path, &header(1600), &[0.25; 1600])["error"], "unavailable");
        assert_eq!(request(&path, "{\"v\":1,\"op\":\"status\"}\n", &[])["error"], "unavailable");
    }
    /// Opt-in, loads the real model (~2 GB; the main checkout's models/ symlinked into the worktree): a 20.8-s
    /// fixture that crosses one window seam, through the socket, against the live engine's golden transcript.
    /// `cargo test -p app --bin heimdall vella_asr::tests::real_model -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_model_transcribes_a_fixture_through_the_socket() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let models = std::env::var_os("HEIMDALL_MODELS").map(PathBuf::from).unwrap_or(root.join("models"));
        let audio = listen::file::read_wav(&root.join("crates/listen/tests/fixtures/continuous.wav")).unwrap();
        let load = Instant::now();
        let worker = Arc::new(spawn_listen_worker(&models, "metal", "metal").unwrap());
        worker.wait_ready().unwrap();
        eprintln!("model ready in {:.1} s", load.elapsed().as_secs_f64());
        let dir = tempfile::tempdir().unwrap();
        let mut asr = VellaAsr::start(dir.path(), false).0;
        asr.sync(Some(worker.clone()));
        let path = asr.socket().to_path_buf();
        assert_eq!(request(&path, "{\"v\":1,\"op\":\"status\"}\n", &[])["ok"], true);
        let normalize = |t: &str| t.to_lowercase().split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()).to_string()).filter(|w| !w.is_empty()).collect::<Vec<_>>();
        let golden = normalize("The aspect ratio of this format dividing by 12 to obtain the simplest whole number ratio, is therefore said to be 3 to 2. Blogs can also help improve student writing, while students often begin their blog experience with sloppy grammar and spelling, the presence of an audience generally changes that.");
        for run in 0..3 {
            let sent = Instant::now();
            let reply = request(&path, &header(audio.len()), &audio);
            let wall = sent.elapsed().as_secs_f64() * 1000.;
            eprintln!("run {run}: client {wall:.0} ms, reply {reply}");
            assert_eq!(reply["ok"], true, "{reply}");
            assert_eq!(reply["windows"], 2, "20.8 s crosses one seam");
            let words = normalize(reply["text"].as_str().unwrap());
            // Word-level edit distance against the live engine's own golden output.
            let mut d: Vec<usize> = (0..=golden.len()).collect();
            for (i, w) in words.iter().enumerate() {
                let mut prev = d[0];
                d[0] = i + 1;
                for (j, g) in golden.iter().enumerate() {
                    let cur = d[j + 1];
                    d[j + 1] = (prev + usize::from(w != g)).min(d[j] + 1).min(cur + 1);
                    prev = cur;
                }
            }
            let errors = d[golden.len()];
            eprintln!("  {errors} word differences from the live engine's golden over {} words", golden.len());
            assert!(errors <= 3, "{words:?}");
            assert_eq!(words.iter().filter(|w| *w == "students").count(), 1, "no seam word twice");
        }
        drop(asr);
        drop(worker);
    }
    #[test]
    fn a_socket_held_by_another_instance_is_not_ready_until_it_starts() {
        let dir = tempfile::tempdir().unwrap();
        let older = Server::start(dir.path().join(SOCKET_NAME), Held::new()).unwrap();
        let (mut asr, error) = VellaAsr::start(dir.path(), false);
        assert!(error.is_some(), "the older instance still answers");
        asr.retry_at = Some(Instant::now());
        asr.sync(None);
        assert!(asr.server.is_none() && asr.retry_at.is_some_and(|t| t > Instant::now()), "retried, rescheduled");
        drop(older);
        asr.retry_at = Some(Instant::now());
        asr.sync(None);
        assert!(asr.server.is_some() && asr.retry_at.is_none());
    }
    #[test]
    fn live_worker_lends_its_model_and_unavailable_without_one() {
        use listen::{onnx::VadState, ListenConfig, ListenEngine, QueueConfig, Recognizer, SpeechDetector, TurnDetector, Word};
        struct Quiet;
        impl SpeechDetector for Quiet {
            fn probability(&mut self, _: &[f32; 512], _: &mut VadState) -> anyhow::Result<f32> { Ok(0.) }
        }
        impl TurnDetector for Quiet {
            fn probability(&mut self, _: &[f32]) -> anyhow::Result<f32> { Ok(0.) }
        }
        /// One word per window, at the window's start: a 20-s request reads "w0 w1" (two windows).
        struct PerWindow;
        impl Recognizer for PerWindow {
            fn transcribe(&mut self, a: &[f32], ns: u64) -> anyhow::Result<Transcript> {
                Ok(Transcript::from_words(vec![Word { text: format!("w{}", ns / 12_960_000_000), start_ns: ns, end_ns: ns + a.len().min(3200) as u64 * 62_500, confidence: 1. }]))
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut asr = VellaAsr::start(dir.path(), false).0;
        let path = asr.socket().to_path_buf();
        asr.sync(None);
        assert!(!asr.is_ready());
        assert_eq!(request(&path, &header(16), &[0.25; 16])["error"], "unavailable", "no listen worker");
        let worker = Arc::new(ListenWorker::spawn(QueueConfig::default(), || ListenEngine::new(PerWindow, Quiet, Quiet, ListenConfig::default())).unwrap());
        worker.wait_ready().unwrap();
        asr.sync(Some(worker.clone()));
        assert!(asr.is_ready());
        let reply = request(&path, &header(320_000), &vec![0.25; 320_000]);
        assert_eq!((reply["ok"].as_bool(), reply["text"].as_str(), reply["windows"].as_u64()), (Some(true), Some("w0 w1"), Some(2)));
        assert_eq!(reply["audio_ms"], 20_000);
        worker.request_shutdown();
        asr.sync(Some(worker.clone()));
        assert!(!asr.is_ready(), "a stopping worker lends nothing");
        assert_eq!(request(&path, &header(16), &[0.25; 16])["error"], "unavailable");
        drop(asr);
        assert!(!path.exists());
    }
}
