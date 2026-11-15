//! Night writes must leave the live archive reserve untouched.
use anyhow::{ensure, Context, Result};
use std::path::Path;
pub const MARGIN: u64 = 32 * 1024 * 1024;
/// Free space night writes leave for the live archive.
pub const DEFAULT_FLOOR: u64 = 10 * 1024 * 1024 * 1024;
pub fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::statvfs(c.as_ptr(), &mut v) } == 0,
        "night free-space probe: {}",
        std::io::Error::last_os_error()
    );
    Ok((v.f_bavail as u64).saturating_mul(v.f_frsize))
}
pub fn require_at(path: &Path, bytes: u64, floor: u64) -> Result<()> {
    require_available(free_bytes(path)?, bytes, floor)
}
fn require_available(available: u64, bytes: u64, floor: u64) -> Result<()> {
    ensure!(
        available >= floor.saturating_add(MARGIN).saturating_add(bytes),
        "night deferred: live disk reserve"
    );
    Ok(())
}
pub fn require(path: &Path, bytes: u64) -> Result<()> {
    require_at(path, bytes, DEFAULT_FLOOR)
}
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    atomic_write_at(path, bytes, DEFAULT_FLOOR)
}
/// `atomic_write` keeping `floor` free (tests prove the refusal with a floor at the disk's free space).
pub fn atomic_write_at(path: &Path, bytes: &[u8], floor: u64) -> Result<()> {
    let parent = path.parent().context("night write parent")?;
    heimdall_archive::private_dir(parent)?;
    require_at(parent, bytes.len() as u64, floor)?;
    heimdall_archive::atomic_write(path, bytes)
}
/// Set a hard SQLite size ceiling before each transaction. Temporary views stay
/// in RAM; the safety margin covers small WAL/metadata writes between checks.
pub fn limit_db(db: &rusqlite::Connection, root: &Path) -> Result<()> {
    limit_db_at(db, root, DEFAULT_FLOOR)
}
/// `limit_db` keeping `floor` free.
pub fn limit_db_at(db: &rusqlite::Connection, root: &Path, floor: u64) -> Result<()> {
    require_at(root, 1024 * 1024, floor)?;
    let page: u64 = db.query_row("PRAGMA page_size", [], |r| {
        r.get::<_, i64>(0).map(|n| n as u64)
    })?;
    let pages: u64 = db.query_row("PRAGMA page_count", [], |r| {
        r.get::<_, i64>(0).map(|n| n as u64)
    })?;
    let extra =
        free_bytes(root)?.saturating_sub(floor.saturating_add(MARGIN)) / page.saturating_mul(2);
    db.pragma_update(
        None,
        "max_page_count",
        (pages.saturating_add(extra).min(4_294_967_294)) as i64,
    )?;
    Ok(())
}
pub const OWNER: &str = "heimdall-night-scratch-v1";
pub fn cleanup(output: &Path) -> Result<()> {
    if !output.exists() {
        return Ok(());
    }
    let lock = scratch_lock(output)?;
    let results = output.join("results");
    let results_owned = std::fs::symlink_metadata(&results).is_ok_and(|m| m.is_dir());
    let _result_lock = if results_owned {
        // The process owns scratch.lock; standalone run owns night.lock. Never
        // reclaim another still-running worker's unpublished files.
        use std::os::fd::AsRawFd;
        let result_lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(results.join(".night.lock"))?;
        ensure!(
            unsafe { libc::flock(result_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "night results still active"
        );
        Some(result_lock)
    } else {
        None
    };
    for entry in std::fs::read_dir(output)? {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && entry
                .file_name()
                .to_string_lossy()
                .starts_with("night-snapshot-")
            && std::fs::read_to_string(entry.path().join("OWNER"))
                .ok()
                .as_deref()
                == Some(OWNER)
        {
            std::fs::remove_dir_all(entry.path())?;
        }
    }
    if results_owned {
        for name in [
            "evidence.jsonl.part",
            "pending-request.json",
            "pending-result.json",
        ] {
            let path = results.join(name);
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
                std::fs::remove_file(path)?;
            }
        }
        for dir in [
            results.clone(),
            results.join("audio"),
            results.join("phases"),
        ] {
            if !std::fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) {
                continue;
            }
            for entry in std::fs::read_dir(dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if owned_part(&name) {
                    std::fs::remove_file(entry.path())?;
                }
            }
        }
    }
    drop(lock);
    Ok(())
}
fn owned_part(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".part") else {
        return false;
    };
    let Some((identity, sequence)) = stem.rsplit_once('.') else {
        return false;
    };
    let Some((pid, number)) = sequence.split_once('-') else {
        return false;
    };
    !pid.is_empty()
        && !number.is_empty()
        && pid
            .bytes()
            .chain(number.bytes())
            .all(|v| v.is_ascii_digit())
        && (matches!(identity, "coverage" | "pending-request" | "pending-result")
            || (identity.len() == 64 && identity.bytes().all(|v| v.is_ascii_hexdigit())))
}
pub fn scratch_lock(output: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    heimdall_archive::private_dir(output)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(output.join(".snapshot.lock"))?;
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "snapshot job already running"
    );
    Ok(lock)
}
#[cfg(test)]
mod tests {
    use super::*;
    /// Review 14 Nov should-fix 6 (restores live_floor_file_stops_temp_writes without the env override): with the
    /// floor at the disk's free space, night writes and the SQLite ceiling refuse and leave nothing behind; with no
    /// floor they go through.
    #[test]
    fn writes_below_the_free_space_floor_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        let floor = free_bytes(root).unwrap();
        assert!(atomic_write_at(&root.join("denied.wav"), &[0; 4096], floor).is_err());
        assert!(!root.join("denied.wav").exists());
        let db = rusqlite::Connection::open_in_memory().unwrap();
        assert!(limit_db_at(&db, root, floor).is_err());
        atomic_write_at(&root.join("allowed.wav"), &[0; 4096], 0).unwrap();
        assert_eq!(std::fs::metadata(root.join("allowed.wav")).unwrap().len(), 4096);
        limit_db_at(&db, root, 0).unwrap();
    }
    #[test]
    fn cleanup_reclaims_pending_parts_but_retains_immutable_receipts() {
        let t = tempfile::tempdir().unwrap();
        let results = t.path().join("results");
        std::fs::create_dir_all(results.join("audio")).unwrap();
        let hash = "a".repeat(64);
        for name in [
            "pending-request.json",
            "pending-result.json",
            "evidence.jsonl.part",
            "coverage.123-0.part",
        ] {
            std::fs::write(results.join(name), b"partial").unwrap();
        }
        std::fs::write(
            results.join("audio").join(format!("{hash}.123-0.part")),
            b"partial",
        )
        .unwrap();
        std::fs::write(
            results.join("audio").join(format!("{hash}.wav")),
            b"durable",
        )
        .unwrap();
        std::fs::write(results.join("unknown.part"), b"other").unwrap();
        use std::os::fd::AsRawFd;
        let active = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(results.join(".night.lock"))
            .unwrap();
        assert_eq!(
            unsafe { libc::flock(active.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(cleanup(t.path()).is_err());
        assert!(results.join("pending-request.json").exists());
        drop(active);
        cleanup(t.path()).unwrap();
        assert!(!results.join("pending-request.json").exists());
        assert!(!results.join("coverage.123-0.part").exists());
        assert!(!results
            .join("audio")
            .join(format!("{hash}.123-0.part"))
            .exists());
        assert_eq!(
            std::fs::read(results.join("audio").join(format!("{hash}.wav"))).unwrap(),
            b"durable"
        );
        assert!(results.join("unknown.part").exists());
    }
    #[test]
    fn reserve_denies_expansion_before_consuming_live_floor() {
        let available = DEFAULT_FLOOR + MARGIN;
        assert!(require_available(available, 1, DEFAULT_FLOOR).is_err());
        assert!(require_available(available + 1, 1, DEFAULT_FLOOR).is_ok());
        assert!(require_available(available, 1, 0).is_ok());
    }
    #[test]
    fn cleanup_reclaims_only_owned_scratch_and_respects_running_lock() {
        let t = tempfile::tempdir().unwrap();
        for (name, owner) in [
            ("night-snapshot-owned", OWNER),
            ("night-snapshot-other", "other"),
            ("results", OWNER),
        ] {
            let p = t.path().join(name);
            std::fs::create_dir(&p).unwrap();
            std::fs::write(p.join("OWNER"), owner).unwrap();
        }
        let lock = scratch_lock(t.path()).unwrap();
        assert!(cleanup(t.path()).is_err());
        drop(lock);
        cleanup(t.path()).unwrap();
        assert!(!t.path().join("night-snapshot-owned").exists());
        assert!(t.path().join("night-snapshot-other").exists());
        assert!(t.path().join("results").exists());
    }
}
