//! Every archive writer at once, under the conditions that broke it on 31 Oct and 1 Nov (lab/archive-hardening).
//!
//! Isolated stores only (temp folders, a temp disk image), no model inference. A SQLite file layer
//! (`support/throttle.rs`) slows every read, write and sync made at background QoS, the way macOS throttled the
//! compactor's I/O on a busy disk while it held the write lock (23:51 hang sample): a step's lock time grows with the
//! pages it touches. Each scenario runs real writers on their own threads, as Heimdall does, and asserts fixed limits:
//! the longest any call on a live path took, that nothing errors or stops, and that nothing is lost.
//!
//! Live paths and their limits: a listen evidence call (`records::Writer::event`) returns in `INSTANT`; a journal
//! event (agent, speech, the live loop) or a payload the agent retires (`store::archive_log`) in `EVENT`. A background
//! step may hold the write lock at most `STEP`. Capture's own commit is tested in `crates/capture` against the real
//! committer. The scenarios run one at a time (`serial`): they measure time.
mod support {
    pub mod throttle;
}
use heimdall_archive::{compact, hash, records, records::DICTIONARY, store, wall_ns, Journal, DAY_NS};
use rusqlite::{params, Connection};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const INSTANT: Duration = Duration::from_millis(50);
const EVENT: Duration = Duration::from_millis(1200);
const STEP: Duration = Duration::from_millis(400);
const SESSION: &str = "session-hardening";

/// Longest call and every error of one writer.
#[derive(Default)]
struct Stats {
    calls: u64,
    worst: Duration,
    errors: Vec<String>,
    /// Calls told "the archive is busy" or "the disk is full" by a writer whose caller keeps its source and tries
    /// again (`kept`); for every other writer those are errors.
    busy: u64,
    kept: bool,
}
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static ONE: Mutex<()> = Mutex::new(());
    ONE.lock().unwrap_or_else(|e| e.into_inner())
}
type Shared = Arc<Mutex<Stats>>;
fn note(stats: &Shared, took: Duration, result: anyhow::Result<()>) {
    let mut s = stats.lock().unwrap();
    s.calls += 1;
    s.worst = s.worst.max(took);
    if let Err(e) = result {
        let e = format!("{e:#}");
        // A retired payload's caller keeps its source and tries again: told busy, or the disk is full.
        if s.kept && (e.contains("the archive is busy") || e.contains("disk is full")) {
            s.busy += 1;
        } else if s.errors.len() < 5 {
            s.errors.push(e);
        }
    }
}
fn timed(stats: &Shared, f: impl FnOnce() -> anyhow::Result<()>) {
    let t = Instant::now();
    let r = f();
    note(stats, t.elapsed(), r);
}

/// A store with one session, one capture segment and `old` records and audio blocks older than a day (compaction work),
/// plus `hot` recent records, written directly in one transaction.
fn world(root: &Path, old: usize, hot: usize) -> PathBuf {
    let db = store::open(root).unwrap();
    let sid = store::ensure_session(&db, SESSION).unwrap();
    let seg = store::create_segment(&db, sid, "capture-hardening", 1_000_000_000, 0, "native").unwrap();
    db.execute("INSERT INTO sources VALUES(?1,'mic','{\"kind\":{\"InputDevice\":{}},\"id\":\"mic\"}',0,NULL)", [seg]).unwrap();
    let now = wall_ns();
    let mut enc = zstd::bulk::Compressor::with_dictionary(1, DICTIONARY).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for i in 0..(old + hot) {
        let wall = if i < old { now - 3 * DAY_NS + i as u64 * 1_000_000 } else { now - 60_000_000_000 + i as u64 };
        let body = serde_json::to_vec(&json!({"model":"lab","input":{"source":"mic","start_ns":i,"end_ns":i+32},"probability":(i % 97) as f64 / 97.0,"pad":"x".repeat(400)})).unwrap();
        let id = hash(&body);
        db.execute("INSERT OR IGNORE INTO payloads VALUES(?1,?2,'json-zstd-dict-v1',?3,NULL)", params![id, body.len() as i64, enc.compress(&body).unwrap()]).unwrap();
        db.execute("INSERT INTO evidence(session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,'model_output','mic','[]',?2,?3,?4)", params![sid, i as i64, wall as i64, id]).unwrap();
    }
    db.execute_batch("COMMIT").unwrap();
    // Old hot audio: one-second blocks of distinct speech-like PCM, each its own hot object.
    for i in 0..(old / 16).max(4) {
        let pcm: Vec<f32> = (0..16000).map(|k| (((k + i * 7919) as f32 * 0.071).sin() * 6000.).round() / 32768.).collect();
        let id = heimdall_archive::audio::put_hot(&db, 16000, 1, &pcm).unwrap();
        let ns = i as u64 * 1_000_000_000;
        db.execute(
            "INSERT INTO blocks(segment,source_id,host_ns,rate,channels,frames,codec,discontinuity,object_id,offset_frames,wall_ns) VALUES(?1,'mic',?2,16000,1,16000,'f32-zstd',0,?3,0,?4)",
            params![seg, ns as i64, id, (now - 3 * DAY_NS + ns) as i64],
        )
        .unwrap();
    }
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    store::session_path(root, SESSION)
}

/// Longest interval a zero-wait writer on its own connection could not take the write lock.
fn probe(root: &Path, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<Duration> {
    let path = root.join(store::INDEX);
    std::thread::spawn(move || {
        let c = Connection::open(path).unwrap();
        c.busy_timeout(Duration::ZERO).unwrap();
        let (mut worst, mut since) = (Duration::ZERO, None::<Instant>);
        while !stop.load(Ordering::Acquire) {
            match c.execute_batch("BEGIN IMMEDIATE; ROLLBACK") {
                Ok(()) => {
                    if let Some(s) = since.take() {
                        worst = worst.max(s.elapsed());
                    }
                }
                Err(_) => {
                    let _ = c.execute_batch("ROLLBACK");
                    since.get_or_insert_with(Instant::now);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        worst.max(since.map_or(Duration::ZERO, |s| s.elapsed()))
    })
}

/// The live writers Heimdall runs, each on its own thread until `stop`: listen evidence (4 sources × 31 frames a
/// second), the agent's journal (opened per event, as the transport recorder does), speech vectors and events, and the
/// agent loop retiring payloads into archived logs.
struct Live {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    listen: Shared,
    agent: Shared,
    speech: Shared,
    retire: Shared,
    sent: Arc<AtomicU64>,
    retired: Arc<Mutex<Vec<String>>>,
    writer: records::Writer,
}
impl Live {
    fn start(_root: &Path, session: &Path) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (listen, agent, speech) = (Shared::default(), Shared::default(), Shared::default());
        let retire = Arc::new(Mutex::new(Stats { kept: true, ..Default::default() }));
        let sent = Arc::new(AtomicU64::new(0));
        let retired = Arc::new(Mutex::new(vec![]));
        let writer = records::Writer::open(session).unwrap();
        let mut threads = vec![];
        {
            let (stop, stats, w, sent) = (stop.clone(), listen.clone(), writer.clone(), sent.clone());
            threads.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Acquire) {
                    for source in 0..4 {
                        let i = n;
                        timed(&stats, || w.event("model_output", &format!("mic:{source}"), &[], i, json!({"model":"silero","n":i,"source":source})));
                        n += 1;
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    std::thread::sleep(Duration::from_millis(32));
                }
            }));
        }
        {
            let (stop, stats, session) = (stop.clone(), agent.clone(), session.to_path_buf());
            threads.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Acquire) {
                    timed(&stats, || Journal::open(&session)?.event("agent_transport_reply", "agent:pi", &[], n, json!({"frame":n})));
                    n += 1;
                    std::thread::sleep(Duration::from_millis(60));
                }
            }));
        }
        {
            let (stop, stats, session) = (stop.clone(), speech.clone(), session.to_path_buf());
            threads.push(std::thread::spawn(move || {
                let journal = Journal::open(&session).unwrap();
                let mut n = 0u32;
                while !stop.load(Ordering::Acquire) {
                    let values: Vec<f32> = (0..4096).map(|k| ((k as u32 + n * 31) as f32 * 0.001).sin()).collect();
                    timed(&stats, || journal.vector(&values).map(|_| ()));
                    timed(&stats, || journal.event("speak_back", "speech", &[], n as u64, json!({"n":n})));
                    n += 1;
                    std::thread::sleep(Duration::from_millis(400));
                }
            }));
        }
        {
            let (stop, stats, session, retired) = (stop.clone(), retire.clone(), session.to_path_buf(), retired.clone());
            threads.push(std::thread::spawn(move || {
                let mut n = 0u64;
                while !stop.load(Ordering::Acquire) {
                    let name = format!("payload-{n}.json");
                    let bytes = serde_json::to_vec(&json!({"retired": n, "pad": "y".repeat(2000)})).unwrap();
                    let t = Instant::now();
                    let r = store::archive_log(&session, &name, &bytes);
                    if r.is_ok() {
                        retired.lock().unwrap().push(name);
                    }
                    note(&stats, t.elapsed(), r);
                    n += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
            }));
        }
        Self { stop, threads, listen, agent, speech, retire, sent, retired, writer }
    }
    /// Stops the writers, flushes listen evidence, and checks every limit (panics after printing them all).
    fn finish(self, label: &str) {
        let failures = self.check(label);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
    fn check(self, label: &str) -> Vec<String> {
        self.stop.store(true, Ordering::Release);
        for t in self.threads {
            t.join().unwrap();
        }
        let mut failures = vec![];
        if let Err(e) = self.writer.flush() {
            failures.push(format!("{label}: listen flush failed: {e:#}"));
        }
        for (name, stats, limit) in [("listen", &self.listen, INSTANT), ("agent", &self.agent, EVENT), ("speech", &self.speech, EVENT), ("retire", &self.retire, EVENT)] {
            let s = stats.lock().unwrap();
            eprintln!("{label}: {name}: {} calls, longest {:?}, {} errors, {} told busy", s.calls, s.worst, s.errors.len(), s.busy);
            if !s.errors.is_empty() {
                failures.push(format!("{label}: {name} failed: {:?}", s.errors));
            }
            if s.calls == 0 {
                failures.push(format!("{label}: {name} never ran"));
            }
            if s.worst > limit {
                failures.push(format!("{label}: {name} waited {:?} on the archive (limit {limit:?})", s.worst));
            }
        }
        failures
    }
}

fn evidence_rows(root: &Path, kind: &str) -> i64 {
    store::open(root).unwrap().query_row("SELECT count(*) FROM evidence WHERE kind=?1", [kind], |r| r.get(0)).unwrap()
}

/// Compaction runs (gate open, background QoS, throttled I/O) while every live writer works. 1 Nov 00:12-00:22: the
/// compactor held the lock for minutes; the agent restarted, speech failed, the evidence writer stopped.
#[test]
fn compaction_never_holds_live_writers() {
    let _one = serial();
    support::throttle::install();
    support::throttle::set_io(Duration::from_micros(300));
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let session = world(&root, 1024, 20_000);
    let stop = Arc::new(AtomicBool::new(false));
    let held = probe(&root, stop.clone());
    let live = Live::start(&root, &session);
    let compactor = compact::Compactor::start_gated(root.clone(), || true).unwrap();
    // Until the old evidence is packed (or 40 s): the step that broke on 31 Oct and 1 Nov.
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(40) {
        let left: i64 = store::open(&root).unwrap().query_row("SELECT count(*) FROM evidence WHERE kind='model_output' AND source='mic'", [], |r| r.get(0)).unwrap();
        if left <= 20_000 {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    std::thread::sleep(Duration::from_secs(2));
    let packed_in = started.elapsed();
    drop(compactor);
    let mut failures = live.check("compaction");
    stop.store(true, Ordering::Release);
    let longest = held.join().unwrap();
    let left: i64 = store::open(&root).unwrap().query_row("SELECT count(*) FROM evidence WHERE kind='model_output' AND source='mic'", [], |r| r.get(0)).unwrap();
    eprintln!("compaction: {} old records left after {packed_in:?}; longest write-lock hold seen {longest:?}", left - 20_000);
    if left != 20_000 {
        failures.push(format!("compaction left {} old records while live writers worked", left - 20_000));
    }
    if longest > STEP {
        failures.push(format!("a writer held the archive's write lock {longest:?} (limit {STEP:?})"));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// Another process (the night pass, a CLI, `heimdall-clip`) holds the write lock for 3 s. Nothing on a live path waits
/// it out, nothing errors, and every write lands afterwards.
#[test]
fn a_second_process_holding_the_lock_stalls_no_live_path() {
    let _one = serial();
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let session = world(&root, 0, 2_000);
    let live = Live::start(&root, &session);
    std::thread::sleep(Duration::from_millis(500));
    let child = hold_in_child(&root, Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(3500));
    child.wait_with_output().map(|o| assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr))).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let sent = live.sent.clone();
    let agent = live.agent.clone();
    let retire = live.retire.clone();
    live.finish("second process");
    assert!(heimdall_archive::live::settle(&root, Duration::from_secs(20)), "the queued writes drain");
    assert_eq!(evidence_rows(&root, "model_output") as u64, 2_000 + sent.load(Ordering::Relaxed), "every listen record committed");
    assert_eq!(evidence_rows(&root, "agent_transport_reply") as u64, agent.lock().unwrap().calls, "every agent record committed");
    let logs: i64 = store::open(&root).unwrap().query_row("SELECT count(*) FROM logs", [], |r| r.get(0)).unwrap();
    assert_eq!(logs as u64, retire.lock().unwrap().calls, "every retired payload archived, those told busy included");
}

/// The write lock, held by a child process for `HARDENING_HOLD_MS` (the test binary re-runs `child_holds_the_lock`).
fn hold_in_child(root: &Path, hold: Duration) -> std::process::Child {
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_holds_the_lock", "--ignored", "--nocapture", "--test-threads=1"])
        .env("HARDENING_ROOT", root)
        .env("HARDENING_HOLD_MS", hold.as_millis().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}
#[test]
#[ignore]
fn child_holds_the_lock() {
    let Ok(root) = std::env::var("HARDENING_ROOT") else { return };
    let hold: u64 = std::env::var("HARDENING_HOLD_MS").unwrap().parse().unwrap();
    let c = Connection::open(Path::new(&root).join(store::INDEX)).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch("BEGIN IMMEDIATE; INSERT INTO store_meta(key,value) VALUES('child','1') ON CONFLICT(key) DO UPDATE SET value=value+1;").unwrap();
    std::thread::sleep(Duration::from_millis(hold));
    c.execute_batch("COMMIT").unwrap();
}

/// A reader holds its snapshot (an export, a clip) while the live writers commit and a tool asks for the store's size:
/// the WAL cannot reset meanwhile, and the size query's checkpoint must not stop the writers (`bytes_on_disk` ran
/// `wal_checkpoint(TRUNCATE)`, which blocks every writer while a reader holds an old snapshot).
#[test]
fn a_long_reader_and_a_size_query_stall_no_writer() {
    let _one = serial();
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    let session = world(&root, 0, 2_000);
    let reader = Connection::open_with_flags(root.join(store::INDEX), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader.query_row("SELECT count(*) FROM evidence", [], |r| r.get(0)).unwrap();
    let live = Live::start(&root, &session);
    std::thread::sleep(Duration::from_millis(300));
    let size = {
        let root = root.clone();
        std::thread::spawn(move || store::bytes_on_disk(&root).map(|_| ()))
    };
    std::thread::sleep(Duration::from_secs(3));
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    size.join().unwrap().unwrap();
    live.finish("long reader");
    // Once the reader is gone the WAL is checkpointed and trimmed (journal_size_limit, 64 MB) by later commits.
    for i in 0..20 {
        Journal::open(&session).unwrap().event("after", "test", &[], i, json!({})).unwrap();
    }
    assert!(heimdall_archive::live::settle(&root, Duration::from_secs(10)));
    let wal = std::fs::metadata(root.join(format!("{}-wal", store::INDEX))).map_or(0, |m| m.len());
    assert!(wal <= 64 << 20, "WAL {wal} bytes after the reader ended");
}

/// The disk fills up while Heimdall listens and the agent works, then space comes back. Nothing stops for good,
/// nothing waits on a live path, and every record kept in memory lands once there is room (31 Oct: an evidence writer
/// error that was not a held lock ended the writer, and every listen event after it failed).
#[test]
fn a_full_disk_stops_nothing_and_loses_nothing_kept() {
    let _one = serial();
    let Some(disk) = SmallDisk::new(48) else {
        eprintln!("hdiutil unavailable: skipped");
        return;
    };
    let root = disk.mount.join("archive");
    let session = world(&root, 0, 500);
    let live = Live::start(&root, &session);
    std::thread::sleep(Duration::from_millis(500));
    // Fill what is left of the disk.
    let filler = disk.mount.join("filler");
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&filler).unwrap();
        let block = vec![0x5au8; 1 << 20];
        while f.write_all(&block).is_ok() {}
        let _ = f.sync_all();
    }
    std::thread::sleep(Duration::from_secs(3));
    std::fs::remove_file(&filler).unwrap();
    std::thread::sleep(Duration::from_secs(4));
    let sent = live.sent.clone();
    live.finish("full disk");
    assert!(heimdall_archive::live::settle(&root, Duration::from_secs(20)), "the kept writes drain once there is room");
    let rows = evidence_rows(&root, "model_output") as u64;
    let dropped: u64 = Journal::open_read_only(&session).unwrap().records().unwrap().iter().filter(|r| r.kind == "diagnostic_drops").map(|r| r.payload["count"].as_u64().unwrap_or(0)).sum();
    assert_eq!(rows + dropped, 500 + sent.load(Ordering::Relaxed), "every listen record committed or counted as dropped");
}

/// A temporary HFS+ disk image of `mb` MB, attached without Finder (`-nobrowse`), detached on drop.
struct SmallDisk {
    _dir: tempfile::TempDir,
    mount: PathBuf,
}
impl SmallDisk {
    fn new(mb: u32) -> Option<Self> {
        let dir = tempfile::tempdir().ok()?;
        let image = dir.path().join("disk.dmg");
        let mount = dir.path().join("mnt");
        std::fs::create_dir(&mount).ok()?;
        let ok = std::process::Command::new("hdiutil").args(["create", "-quiet", "-size", &format!("{mb}m"), "-fs", "HFS+", "-volname", "hardening", "-layout", "NONE"]).arg(&image).status().ok()?.success()
            && std::process::Command::new("hdiutil").args(["attach", "-quiet", "-nobrowse", "-noverify", "-mountpoint"]).arg(&mount).arg(&image).status().ok()?.success();
        ok.then_some(Self { _dir: dir, mount })
    }
}
impl Drop for SmallDisk {
    fn drop(&mut self) {
        store::close_packs();
        let _ = std::process::Command::new("hdiutil").args(["detach", "-quiet", "-force"]).arg(&self.mount).status();
    }
}

/// A compaction step killed at each crash point (`heimdall-archive compact-file --crash-after`), with live writes
/// committed around it: the store opens, passes `integrity_check`, keeps every committed record, and the next pass
/// finishes the work.
#[test]
fn a_crash_mid_step_loses_nothing_committed_and_the_work_resumes() {
    let _one = serial();
    for stage in ["verified", "committed", "chunk_verified", "chunk_committed"] {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("archive");
        let session = world(&root, 256, 200);
        Journal::open(&session).unwrap().event("before_crash", "test", &[], 1, json!({})).unwrap();
        let now = wall_ns().to_string();
        let args: Vec<&str> = if stage.starts_with("chunk") {
            vec!["pack-file", root.to_str().unwrap(), &now, "records", "--crash-after", stage]
        } else {
            vec!["compact-file", root.to_str().unwrap(), &now, "--crash-after", stage]
        };
        let status = std::process::Command::new(env!("CARGO_BIN_EXE_heimdall-archive"))
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success(), "{stage}: the child was killed");
        let db = store::open(&root).unwrap();
        let check: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(check, "ok", "{stage}");
        assert_eq!(evidence_rows(&root, "before_crash"), 1, "{stage}: a committed record survives");
        Journal::open(&session).unwrap().event("after_crash", "test", &[], 2, json!({})).unwrap();
        for _ in 0..40 {
            compact::compact_once(&root, wall_ns()).unwrap();
        }
        let old: i64 = db.query_row("SELECT count(*) FROM evidence WHERE wall_ns<=?1", [(wall_ns() - DAY_NS) as i64], |r| r.get(0)).unwrap();
        assert_eq!(old, 0, "{stage}: the next passes pack every old record");
        let hot: i64 = db.query_row("SELECT count(*) FROM blocks WHERE codec='f32-zstd' AND wall_ns<=?1", [(wall_ns() - DAY_NS) as i64], |r| r.get(0)).unwrap();
        assert_eq!(hot, 0, "{stage}: and every old audio block");
        let mut all = 0;
        Journal::open_read_only(&session).unwrap().for_each_record(|_| { all += 1; Ok(()) }).unwrap();
        assert_eq!(all, 256 + 200 + 2, "{stage}: every record readable, packed or live");
    }
}

/// Opt-in, realistic size (lab/archive-hardening/REPORT.md): an index of `HARDENING_BIG_GB` GB (default 11.5, the size
/// of 31 Oct) with old work for the compactor, a reader holding its snapshot for 20 s (an export, a clip), every live
/// writer and the compactor for a minute, the throttle model on. Same limits as the small scenarios; afterwards the WAL
/// is back within its 64 MB limit. Writes the index to a temp folder and deletes it.
/// `cargo test -p heimdall-archive --release --test hardening -- --ignored --nocapture realistic_size`
#[test]
#[ignore]
fn realistic_size_with_a_long_reader() {
    let _one = serial();
    support::throttle::install();
    support::throttle::set_io(Duration::from_micros(300));
    let gb: f64 = std::env::var("HARDENING_BIG_GB").ok().and_then(|v| v.parse().ok()).unwrap_or(11.5);
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("archive");
    // The same compaction work as `compaction_never_holds_live_writers`: only the index's size differs.
    let session = world(&root, 1024, 2_000);
    let started = Instant::now();
    {
        // Bulk rows (bodies that do not compress, like real JSON evidence after zstd), in key order, until the index
        // reaches the size.
        let db = store::open(&root).unwrap();
        let sid: i64 = db.query_row("SELECT id FROM sessions WHERE name=?1", [SESSION], |r| r.get(0)).unwrap();
        let target = (gb * (1u64 << 30) as f64) as u64;
        let now = wall_ns();
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut body = vec![0u8; 2400];
        let mut n = 0u64;
        while std::fs::metadata(root.join(store::INDEX)).unwrap().len() < target {
            db.execute_batch("BEGIN").unwrap();
            {
                let mut payload = db.prepare_cached("INSERT INTO payloads VALUES(?1,?2,'json-zstd-dict-v1',?3,NULL)").unwrap();
                let mut evidence = db.prepare_cached("INSERT INTO evidence(session,kind,source,utterance_ids,host_ns,wall_ns,payload_id) VALUES(?1,'model_output','bulk','[]',?2,?3,?4)").unwrap();
                for _ in 0..50_000 {
                    for b in body.iter_mut() {
                        seed ^= seed << 13;
                        seed ^= seed >> 7;
                        seed ^= seed << 17;
                        *b = seed as u8;
                    }
                    let id = format!("bulk{n:060}");
                    payload.execute(params![id, 4000, body]).unwrap();
                    evidence.execute(params![sid, n as i64, (now - 3_600_000_000_000 + n) as i64, id]).unwrap();
                    n += 1;
                }
            }
            db.execute_batch("COMMIT; PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        }
        eprintln!("realistic: index {:.1} GB, {n} bulk rows, built in {:.0} s", std::fs::metadata(root.join(store::INDEX)).unwrap().len() as f64 / (1u64 << 30) as f64, started.elapsed().as_secs_f64());
    }
    let reader = Connection::open_with_flags(root.join(store::INDEX), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    let _: i64 = reader.query_row("SELECT count(*) FROM evidence WHERE source='mic'", [], |r| r.get(0)).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let held = probe(&root, stop.clone());
    let live = Live::start(&root, &session);
    let compactor = compact::Compactor::start_gated(root.clone(), || true).unwrap();
    std::thread::sleep(Duration::from_secs(20));
    let wal_held = std::fs::metadata(root.join(format!("{}-wal", store::INDEX))).map_or(0, |m| m.len());
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    let until = Instant::now() + Duration::from_secs(100);
    let old = |root: &Path| -> i64 { store::open(root).unwrap().query_row("SELECT count(*) FROM evidence WHERE source='mic' AND wall_ns<=?1", [(wall_ns() - DAY_NS) as i64], |r| r.get(0)).unwrap() };
    while Instant::now() < until && old(&root) > 0 {
        std::thread::sleep(Duration::from_millis(500));
    }
    let left = old(&root);
    drop(compactor);
    let mut failures = live.check("realistic");
    stop.store(true, Ordering::Release);
    let longest = held.join().unwrap();
    // Commits after the reader left: the checkpointer runs, and the WAL is trimmed to its limit.
    for i in 0..3000 {
        Journal::open(&session).unwrap().note("after", "test", &[], i, json!({"pad": "z".repeat(800)})).unwrap();
    }
    assert!(heimdall_archive::live::settle(&root, Duration::from_secs(30)));
    std::thread::sleep(Duration::from_secs(3));
    let wal_after = std::fs::metadata(root.join(format!("{}-wal", store::INDEX))).map_or(0, |m| m.len());
    eprintln!("realistic: WAL {:.0} MB while the reader held its snapshot, {:.0} MB after; {left} old records left; longest write-lock hold {longest:?}", wal_held as f64 / 1e6, wal_after as f64 / 1e6);
    if left != 0 {
        failures.push(format!("compaction left {left} old records"));
    }
    if longest > STEP {
        failures.push(format!("a writer held the archive's write lock {longest:?} (limit {STEP:?})"));
    }
    if wal_after > 64 << 20 {
        failures.push(format!("the WAL stayed at {wal_after} bytes after the reader ended"));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
