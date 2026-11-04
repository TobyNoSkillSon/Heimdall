//! The single archive store (index + packs): capture objects, model evidence, transcript,
//! incremental cold compaction, export, and migration from the per-launch folder layout.
pub mod audio;
pub mod chunks;
pub mod clip;
pub mod compact;
pub mod export;
pub mod embedding;
pub mod legacy;
pub mod live;
pub mod migrate;
pub mod records;
pub mod diagnostics;
pub mod event_timeline;
pub mod snapshot;
pub mod timeline;
pub mod store;
pub mod video;
pub use snapshot::View;
mod opus;
use anyhow::{Result, ensure};
pub use records::{Journal, Record};
/// Evidence kind of one spoken bug note: the note, its trigger and the context
/// window snapshot (transcript, decisions, commands, screen and audio references).
pub const BUG_NOTE_KIND: &str = "bug_note";
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::Path,
};

pub const DAY_NS: u64 = 86_400_000_000_000;
pub fn wall_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
/// Flush OS metadata AND device write caches on macOS. Failure is fatal to
/// compaction: callers retain originals rather than silently weaken durability.
pub fn full_sync(file: &File) -> Result<()> {
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
/// Same-directory durable replace. An interrupted temp never becomes a referenced object.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("object parent"))?;
    private_dir(parent)?;
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "{}-{}.part",
        std::process::id(),
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(bytes)?;
    full_sync(&file)?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    full_sync(&File::open(parent)?)?;
    Ok(())
}
pub(crate) fn object_path(dir: &Path, relative: &str) -> Result<std::path::PathBuf> {
    ensure!(
        !relative.is_empty()
            && Path::new(relative)
                .components()
                .all(|p| matches!(p, std::path::Component::Normal(_))),
        "invalid archive object path"
    );
    Ok(dir.join(relative))
}

/// Lossless log rotation with an independent decode check before publication.
pub fn compress_log(bytes: &[u8]) -> Result<Vec<u8>> {
    let packed = zstd::stream::encode_all(bytes, 9)?;
    ensure!(zstd::stream::decode_all(packed.as_slice())? == bytes, "log rotation checksum mismatch");
    Ok(packed)
}
pub fn decompress_log(bytes: &[u8]) -> Result<Vec<u8>> { Ok(zstd::stream::decode_all(bytes)?) }

