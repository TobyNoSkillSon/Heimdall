use crate::settings::{private_dir, Settings};
use anyhow::{ensure, Result};
use capture::{ArchiveSink, NativeFormat, SourceId, SourceInfo};
use std::{
    collections::HashMap,
    ffi::CString,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::statvfs(c.as_ptr(), &mut v) } == 0,
        "disk free-space check: {}",
        std::io::Error::last_os_error()
    );
    Ok((v.f_bavail as u64).saturating_mul(v.f_frsize))
}
/// Capture stops at the free-space floor (plus a 16 MiB margin), so battery compaction
/// starts this far above it, before recording would stop.
pub const COMPACT_DISK_MARGIN: u64 = 2 * 1024 * 1024 * 1024;
/// Cold-archive compaction runs on AC power; on battery only when free disk is near the
/// floor, because a full disk stops the archive. Unknown power counts as battery.
/// Compaction runs while the Mac is idle on AC power (no input for `heimdall_night::gate::IDLE_SECONDS`), or when the
/// disk nears its floor. `idle_on_ac`: power and input state, failing closed.
pub fn compaction_allowed(idle_on_ac: Result<bool>, free: Result<u64>, floor: u64) -> bool {
    idle_on_ac.unwrap_or(false)
        || free.is_ok_and(|free| free < floor.saturating_add(COMPACT_DISK_MARGIN))
}
/// The live gate handed to the archive compactor: power source and idle time, free space, current floor.
pub fn compaction_gate(
    root: PathBuf,
    floor: Arc<std::sync::atomic::AtomicU64>,
) -> impl FnMut() -> bool + Send + 'static {
    move || {
        compaction_allowed(
            heimdall_night::gate::state().map(|s| s.eligible()),
            free_bytes(&root),
            floor.load(std::sync::atomic::Ordering::Acquire),
        )
    }
}
/// Check every raw write. Failure to read free space also stops archival.
pub struct DiskGuard {
    pub dir: PathBuf,
    pub floor: u64,
    pub live_floor: Option<Arc<std::sync::atomic::AtomicU64>>,
    pub status: Arc<Mutex<Option<String>>>,
}
impl DiskGuard {
    pub fn floor(&self) -> u64 {
        self.live_floor.as_ref().map_or(self.floor, |f| f.load(std::sync::atomic::Ordering::Acquire))
    }
    pub fn allows(&self, bytes: u64) -> bool {
        let floor = self.floor();
        let result = free_bytes(&self.dir);
        let why = match result {
            Ok(n)
                if n >= floor.saturating_add(bytes)
                    .saturating_add(16 * 1024 * 1024) =>
            {
                None
            }
            Ok(n) => Some(format!(
                "Archive stopped: {:.1} GiB free; {:.1} GiB floor",
                n as f64 / 1073741824.,
                floor as f64 / 1073741824.
            )),
            Err(e) => Some(format!("Archive stopped: {e}")),
        };
        let allowed = why.is_none();
        *self.status.lock().unwrap() = why;
        allowed
    }
}
/// Retention now means permanent cold storage. The shared Compactor owns age transitions.
pub fn prune(_root: &Path, _days: u32, _active: Option<&Path>) -> Result<usize> {
    Ok(0)
}
/// A new session's address in the archive store, `<archive>/session-<ms>-<uuid>`. It names a
/// session inside the store and is never a directory.
pub fn session_dir(root: &Path) -> Result<PathBuf> {
    private_dir(root)?;
    Ok(root.join(format!("session-{}-{}", agent::now_ms(), uuid::Uuid::new_v4())))
}
/// The archive store root a session address belongs to.
pub fn store_root(session: &Path) -> PathBuf {
    session.parent().map(Path::to_path_buf).unwrap_or_default()
}
/// Per-launch working files that are not archive content (agent dry-run log and spool, temporary
/// screenshot PNGs, the decide sink): `<data>/run/<session-name>/`.
pub fn run_dir(data: &Path, session: &Path) -> PathBuf {
    data.join("run").join(session.file_name().unwrap_or_default())
}
/// Archives a launch's run directory into its session's logs (non-empty files; SQLite files with
/// their committed WAL) and removes it.
pub fn retire_run(run: &Path, session: &Path) -> Result<()> {
    if !run.is_dir() {
        return Ok(());
    }
    fn files(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let t = e.file_type()?;
            if t.is_dir() {
                files(base, &e.path(), out)?;
            } else if t.is_file() {
                out.push((e.path().strip_prefix(base)?.to_string_lossy().to_string(), e.path()));
            }
        }
        Ok(())
    }
    let mut all = vec![];
    files(run, run, &mut all)?;
    for (name, path) in all {
        if name.ends_with("-wal") || name.ends_with("-shm") {
            continue;
        }
        let bytes = if name.ends_with(".sqlite") { heimdall_archive::store::sqlite_bytes(&path)? } else { std::fs::read(&path)? };
        if !bytes.is_empty() {
            heimdall_archive::store::archive_log(session, &name, &bytes)?;
        }
    }
    std::fs::remove_dir_all(run)?;
    Ok(())
}
/// Archives and removes run directories that earlier launches left behind (a crash or kill).
pub fn retire_stale_runs(data: &Path, archive_root: &Path, current: &Path) -> Result<usize> {
    let runs = data.join("run");
    if !runs.is_dir() {
        return Ok(0);
    }
    let mut n = 0;
    for e in std::fs::read_dir(&runs)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().to_string();
        if !e.file_type()?.is_dir() || !name.starts_with("session-") || Some(e.file_name().as_os_str()) == current.file_name() {
            continue;
        }
        retire_run(&e.path(), &archive_root.join(&name))?;
        n += 1;
    }
    Ok(n)
}
/// All sources retain native PCM for 24 hours; the shared archive crate owns cold tiers.
pub struct RawArchive {
    /// Created at the first write, on the capture intake thread. Creating a segment waits for the archive's write
    /// lock (up to `store::BUSY`, 60 s), and the Listening on that builds this adapter must never wait for it
    /// (24 Oct 02:30: a compaction step held the lock and Listening on froze every later click for 56 s).
    native: Option<capture::Archive>,
    session: PathBuf,
    guard: DiskGuard,
    // Reserve at most 64 KiB from a fresh probe. The existing 16-MiB margin
    // covers concurrent writers between probes; credits expire after one second.
    space_credit: u64,
    credit_floor: u64,
    credit_at: std::time::Instant,
}
impl RawArchive {
    pub fn with_floor_source(mut self, floor: Arc<std::sync::atomic::AtomicU64>) -> Self {
        self.guard.live_floor = Some(floor); self
    }
    /// Native capture into `session` (`<archive>/<session-name>`).
    pub fn new(
        session: &Path,
        settings: &Settings,
        status: Arc<Mutex<Option<String>>>,
        _enabled: Arc<Mutex<HashMap<String, bool>>>,
    ) -> Result<Self> {
        let root = store_root(session);
        private_dir(&root)?;
        let guard = DiskGuard {
            live_floor: None,
            dir: root,
            floor: settings.free_space_floor_bytes,
            status,
        };
        ensure!(
            guard.allows(1024 * 1024),
            "insufficient free space for archival"
        );
        Ok(Self {
            native: None,
            session: session.to_path_buf(),
            guard,
            space_credit: 0,
            credit_floor: 0,
            credit_at: std::time::Instant::now(),
        })
    }
    fn native(&mut self) -> Result<&mut capture::Archive> {
        if self.native.is_none() {
            self.native = Some(capture::Archive::create(&self.session)?);
        }
        Ok(self.native.as_mut().expect("created above"))
    }
    fn require_space(&mut self, bytes: u64) -> Result<()> {
        if self.credit_floor == self.guard.floor() && self.credit_at.elapsed() < Duration::from_secs(1) && self.space_credit >= bytes {
            self.space_credit -= bytes;
            return Ok(());
        }
        self.space_credit = 0;
        // Reserve the next small group before admitting its first packet. Near
        // the floor, fall back to the original per-write probe without credit.
        let reserve = bytes.saturating_add(64 * 1024);
        if self.guard.allows(reserve) {
            self.space_credit = 64 * 1024;
            self.credit_floor = self.guard.floor();
            self.credit_at = std::time::Instant::now();
        } else {
            ensure!(self.guard.allows(bytes), "archive stopped below free-space floor");
        }
        Ok(())
    }
}
impl ArchiveSink for RawArchive {
    fn add_source(&mut self, i: &SourceInfo, ns: u64) -> Result<()> {
        self.require_space(65536)?;
        self.native()?.add_source(i, ns)
    }
    fn remove_source(&mut self, id: &SourceId, ns: u64) -> Result<()> {
        self.require_space(65536)?;
        self.native()?.remove_source(id, ns)
    }
    fn gap(&mut self, id: Option<&SourceId>, ns: u64, detail: &str) -> Result<()> {
        self.require_space(4096)?;
        self.native()?.gap(id, ns, detail)
    }
    fn append(
        &mut self,
        id: &SourceId,
        ns: u64,
        f: NativeFormat,
        s: &[f32],
        discontinuity: bool,
    ) -> Result<()> {
        // Source toggles govern listening/decisions, while the capture archive retains all input.
        self.require_space(s.len() as u64 * 8 + 4096)?;
        self.native()?.append(id, ns, f, s, discontinuity)
    }
    fn flush(&mut self) -> Result<()> {
        // Nothing written yet: no segment to commit.
        self.native.as_mut().map_or(Ok(()), |n| n.flush())
    }
    fn commit_wait(&self) -> Option<Duration> {
        self.native.as_ref().and_then(ArchiveSink::commit_wait)
    }
}
pub fn mono_16k(samples: &[f32], f: NativeFormat) -> Result<Vec<f32>> {
    crate::audio::mono_16k(samples, f.sample_rate, f.channels)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ArchiveFormat;
    #[test]
    fn compaction_runs_on_ac_and_on_battery_only_near_the_disk_floor() {
        let gib = 1024 * 1024 * 1024u64;
        let floor = 10 * gib;
        // AC: always, whatever the disk says.
        assert!(compaction_allowed(Ok(true), Ok(500 * gib), floor));
        assert!(compaction_allowed(Ok(true), Err(anyhow::anyhow!("statvfs")), floor));
        // Battery with plenty of space waits.
        assert!(!compaction_allowed(Ok(false), Ok(500 * gib), floor));
        assert!(!compaction_allowed(Ok(false), Ok(floor + COMPACT_DISK_MARGIN), floor));
        // Battery near or below the floor runs: capture stops at the floor otherwise.
        assert!(compaction_allowed(Ok(false), Ok(floor + COMPACT_DISK_MARGIN - 1), floor));
        assert!(compaction_allowed(Ok(false), Ok(floor - 1), floor));
        assert!(compaction_allowed(Ok(false), Ok(0), floor));
        // Unknown power counts as battery; unknown space does not open the gate.
        assert!(!compaction_allowed(Err(anyhow::anyhow!("power")), Ok(500 * gib), floor));
        assert!(compaction_allowed(Err(anyhow::anyhow!("power")), Ok(gib), floor));
        assert!(!compaction_allowed(Ok(false), Err(anyhow::anyhow!("statvfs")), floor));
        // The floor is read live: raising it above free space opens the gate on battery.
        let t = tempfile::tempdir().unwrap();
        let live = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut gate = compaction_gate(t.path().into(), live.clone());
        // The live gate wants power and an idle Mac (`eligible`), so the expectation reads the same state: comparing with
        // power alone failed whenever someone was using the Mac (night check, 4 Nov 17:02).
        let eligible = heimdall_night::gate::state().is_ok_and(|s| s.eligible());
        assert_eq!(gate(), eligible);
        live.store(u64::MAX - COMPACT_DISK_MARGIN, std::sync::atomic::Ordering::Release);
        assert!(gate());
    }
    #[test]
    fn disk_floor_is_visible() {
        let t = tempfile::tempdir().unwrap();
        let g = DiskGuard {
            live_floor: None,
            dir: t.path().into(),
            floor: u64::MAX,
            status: Arc::new(Mutex::new(None)),
        };
        assert!(!g.allows(1));
        assert!(g
            .status
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("Archive stopped"));
    }
    #[test]
    fn all_choices_and_disabled_sources_retain_native_bits_disk_failure_stops() {
        for format in [
            ArchiveFormat::NativeLossless,
            ArchiveFormat::Flac24,
            ArchiveFormat::Flac16kMono,
        ] {
            let t = tempfile::tempdir().unwrap();
            let settings = Settings {
                archive_format: format,
                ..Settings::default()
            };
            let enabled = Arc::new(Mutex::new(HashMap::new()));
            let status = Arc::new(Mutex::new(None));
            let i = crate::fake_source("mic", 0, 1);
            enabled.lock().unwrap().insert(i.id.0.clone(), false);
            let mut raw = RawArchive::new(&t.path().join("session-raw"), &settings, status.clone(), enabled).unwrap();
            raw.add_source(&i, 0).unwrap();
            let pcm = [0.1, -0., f32::from_bits(1)];
            raw.append(&i.id, 0, i.format, &pcm, false).unwrap();
            raw.flush().unwrap();
            assert_eq!(
                raw.native.as_ref().unwrap().read_blocks(&i.id).unwrap()[0]
                    .samples
                    .iter()
                    .map(|x| x.to_bits())
                    .collect::<Vec<_>>(),
                pcm.iter().map(|x| x.to_bits()).collect::<Vec<_>>()
            );
            raw.guard.floor = u64::MAX;
            assert!(raw.append(&i.id, 1_000_000, i.format, &pcm, false).is_err());
            assert!(status.lock().unwrap().is_some());
            assert!(!t.path().join("flac.sqlite").exists());
            assert!(!t.path().join("session-raw").exists(), "a session is a row in the store, not a folder");
        }
    }
    #[test]
    fn raw_capacity_credit_is_bounded_expires_and_floor_change_invalidates_it() {
        let t = tempfile::tempdir().unwrap();
        let mut raw = RawArchive::new(&t.path().join("session-raw"), &Settings::default(), Arc::new(Mutex::new(None)), Arc::new(Mutex::new(HashMap::new()))).unwrap();
        raw.require_space(4096).unwrap();
        assert_eq!(raw.space_credit, 65536);
        let checked = raw.credit_at;
        raw.require_space(4096).unwrap();
        assert_eq!(raw.space_credit, 61440);
        assert_eq!(raw.credit_at, checked);
        raw.credit_at = std::time::Instant::now() - Duration::from_secs(2);
        raw.require_space(4096).unwrap();
        assert_eq!(raw.space_credit, 65536);
        assert!(raw.credit_at > checked);
        raw.guard.floor = u64::MAX;
        assert!(raw.require_space(1).is_err());
        assert_eq!(raw.space_credit, 0);
    }
    /// 24 Oct 02:30: Listening on built this adapter on the live loop while a compaction step held the archive's
    /// write lock; creating the capture segment waited 56 s and every later click queued behind it.
    #[test]
    fn listening_on_does_not_wait_for_a_held_archive_lock() {
        let t = tempfile::tempdir().unwrap();
        let session = t.path().join("session-raw");
        let holder = heimdall_archive::store::open(&store_root(&session)).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        let mut raw = RawArchive::new(&session, &Settings::default(), Arc::new(Mutex::new(None)), Arc::new(Mutex::new(HashMap::new()))).unwrap();
        assert!(started.elapsed() < Duration::from_secs(1), "built in {:?} under a held lock", started.elapsed());
        raw.flush().unwrap();
        assert_eq!(raw.commit_wait(), None);
        holder.execute_batch("COMMIT").unwrap();
        // The first write, on the intake thread, creates the segment.
        raw.gap(None, capture::host_now_ns(), "test").unwrap();
        raw.flush().unwrap();
        let segments: i64 = holder.query_row("SELECT count(*) FROM segments", [], |r| r.get(0)).unwrap();
        assert_eq!(segments, 1);
    }
    #[test]
    fn live_disk_floor_reaches_existing_raw_writer_and_invalidates_credit() {
        use std::sync::atomic::{AtomicU64,Ordering};
        let t=tempfile::tempdir().unwrap();let floor=Arc::new(AtomicU64::new(1024*1024*1024));
        let mut raw=RawArchive::new(&t.path().join("session-raw"),&Settings::default(),Arc::new(Mutex::new(None)),Arc::new(Mutex::new(HashMap::new()))).unwrap().with_floor_source(floor.clone());
        raw.require_space(16).unwrap();assert!(raw.space_credit>0);
        floor.store(u64::MAX,Ordering::Release);assert!(raw.require_space(16).is_err());
        floor.store(1024*1024*1024,Ordering::Release);raw.require_space(16).unwrap();
        assert_eq!(raw.credit_floor,1024*1024*1024);
    }
    #[test]
    fn retention_leaves_active_unknown_and_symlinks() {
        let t = tempfile::tempdir().unwrap();
        private_dir(&t.path().join("unowned")).unwrap();
        private_dir(&t.path().join("session-open")).unwrap();
        assert_eq!(prune(t.path(), 1, None).unwrap(), 0);
        assert!(t.path().join("session-open").exists());
    }
    #[test]
    fn run_directories_are_archived_into_their_session_and_removed() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        let root = data.join("archive");
        let crashed = root.join("session-1-crashed");
        let current = root.join("session-2-current");
        for s in [&crashed, &current] {
            let run = run_dir(&data, s);
            private_dir(&run.join("shots")).unwrap();
            std::fs::write(run.join("dry-run.jsonl"), format!("{{\"session\":\"{}\"}}\n", s.display())).unwrap();
            std::fs::write(run.join("shots/empty.png"), b"").unwrap();
            let spool = rusqlite::Connection::open(run.join("dry-run-spool.sqlite")).unwrap();
            spool.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE q(x); INSERT INTO q VALUES('queued');").unwrap();
        }
        assert_eq!(retire_stale_runs(&data, &root, &current).unwrap(), 1);
        assert!(!run_dir(&data, &crashed).exists());
        assert!(run_dir(&data, &current).exists(), "the running launch keeps its working files");
        assert!(heimdall_archive::store::read_log(&crashed, "dry-run.jsonl").unwrap().starts_with(b"{\"session\""));
        let spool = heimdall_archive::store::read_log(&crashed, "dry-run-spool.sqlite").unwrap();
        assert!(spool.starts_with(b"SQLite format 3"));
        assert_eq!(heimdall_archive::store::logs(&crashed).unwrap().len(), 2, "empty files are not archived");
        retire_run(&run_dir(&data, &current), &current).unwrap();
        assert!(!data.join("run").read_dir().unwrap().any(|_| true));
    }
}
