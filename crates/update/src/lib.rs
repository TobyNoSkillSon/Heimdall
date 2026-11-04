//! Heimdall Alpha's updater (docs/updates.md), a port of Vella's design with a signed checksum file.
//!
//! The app side: [`source`] and [`fetch`] find the newest release the running build may take ([`release::choose`]),
//! [`prepare::prepare`] downloads it into a private temp dir and checks it (Ed25519 signature over SHA256SUMS, the
//! zip's SHA-256, archive entries, bundle ID and version, code signature against the running app, quarantine
//! cleared), then [`install::hand_off`] writes a plan and starts `heimdall-install-tool`, and the app quits.
//! The tool side: [`install::Installer`] waits for the app to exit, takes the lock, swaps with two renames, launches
//! the new app and waits for it to report ready ([`readiness`]); otherwise it restores the previous app. Exactly one
//! rollback is kept beside the destination. [`install::recover`] repairs an interrupted swap at the next start.
//! [`skills`] carries bundled skill updates into the profile without overwriting a user's edits.

pub mod bundle;
pub mod fetch;
pub mod install;
pub mod prepare;
pub mod readiness;
pub mod release;
pub mod signing;
pub mod skills;
pub mod source;
pub mod state;
pub mod version;

#[cfg(test)]
mod tests;

pub use release::Release;
pub use version::Version;

/// Why an update did not happen, in words for the alert and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateError(pub String);

impl UpdateError {
    pub fn new(message: impl Into<String>) -> Self {
        UpdateError(message.into())
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UpdateError {}

impl From<std::io::Error> for UpdateError {
    fn from(error: std::io::Error) -> Self {
        UpdateError(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, UpdateError>;

/// What a release bundle must be. [`Product::ALPHA`] is Heimdall Alpha as `scripts/bundle.sh --edition public`
/// builds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Product {
    /// The bundle's folder name, in the zip and in the Applications folder.
    pub app_folder: &'static str,
    pub bundle_id: &'static str,
    /// `Contents/MacOS/<executable>`: the process whose pid the readiness file names.
    pub executable: &'static str,
    /// Paths under `Contents/` that must be executable files.
    pub required: &'static [&'static str],
    /// The zip's name prefix: `<prefix>-<version>-arm64.zip`.
    pub zip_prefix: &'static str,
}

impl Product {
    pub const ALPHA: Product = Product {
        app_folder: "Heimdall Alpha.app",
        bundle_id: "com.tobynoskillson.heimdall",
        executable: "Heimdall",
        required: &[
            "MacOS/Heimdall",
            "MacOS/heimdall-speakers-worker",
            "MacOS/heimdall-clip",
            "MacOS/heimdall-emergency",
            "Helpers/heimdall-install-tool",
        ],
        zip_prefix: "Heimdall-Alpha",
    };

    pub fn zip_name(&self, version: &Version) -> String {
        format!("{}-{}-arm64.zip", self.zip_prefix, version)
    }
}

/// Atomic file write: a temp sibling, fsync, rename.
pub(crate) fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().ok_or_else(|| UpdateError::new("path without a parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        std::process::id()
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Lowercase hex SHA-256 of a file, read in 1 MB blocks.
pub fn sha256_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
