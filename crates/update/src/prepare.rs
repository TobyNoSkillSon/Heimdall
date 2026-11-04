//! Download and check a release before anything is stopped or moved. Leaves nothing behind on failure.
//!
//! Order: SHA256SUMS and its signature (checked against the compiled-in keys), then the zip and its SHA-256 against
//! the one line naming it, then the archive's entry list, then unpack with ditto, then the bundle (ID, version,
//! required executables, no external links), then its code signature against the running app's, then the
//! quarantine attribute is cleared. All of it in a private `heimdall-update.XXXXXX` folder under the temp dir.

use crate::bundle::{self, IdentityCheck};
use crate::fetch::{Fetch, MAX_SMALL, MAX_ZIP};
use crate::release::{archive_entries_are_safe, expected_sha256, Release, SUMS, SUMS_SIG};
use crate::source::Source;
use crate::{signing, Product, Result, UpdateError};
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A downloaded release, checked and unpacked, ready to install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub version: String,
    /// The private work folder (zip, SHA256SUMS, the unpacked app); removed after installing.
    pub directory: PathBuf,
    /// The unpacked, verified app inside `directory`.
    pub app: PathBuf,
    pub sha256: String,
}

pub struct Prepare<'a> {
    pub product: Product,
    pub source: Source,
    pub fetch: &'a dyn Fetch,
    pub keys: &'a [[u8; 32]],
    pub identity: IdentityCheck,
    /// The running app's bundle, whose signature the download must match.
    pub running_app: PathBuf,
    /// Where the private work folder is made (the temp dir in the app).
    pub work_root: PathBuf,
}

impl Prepare<'_> {
    pub fn run(&self, release: &Release, log: &mut dyn FnMut(&str)) -> Result<Staged> {
        let directory = make_work_dir(&self.work_root)?;
        match self.stage(release, &directory, log) {
            Ok(staged) => Ok(staged),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&directory);
                Err(error)
            }
        }
    }

    fn stage(&self, release: &Release, directory: &Path, log: &mut dyn FnMut(&str)) -> Result<Staged> {
        let zip_name = self.product.zip_name(&release.version);
        let url = |asset: &str| self.source.asset_url(&release.tag, asset);
        let sums = self.fetch.get(&url(SUMS), MAX_SMALL)?;
        let sig = self.fetch.get(&url(SUMS_SIG), MAX_SMALL)?;
        signing::verify_sums(&sums, &String::from_utf8_lossy(&sig), self.keys)?;
        std::fs::write(directory.join(SUMS), &sums)?;
        let expected = expected_sha256(&String::from_utf8_lossy(&sums), &zip_name)
            .ok_or_else(|| UpdateError::new(format!("missing or ambiguous SHA-256 for {zip_name}")))?;
        let zip = directory.join(&zip_name);
        self.fetch.download(&url(&zip_name), &zip, MAX_ZIP)?;
        log(&format!("downloaded {zip_name}"));
        let actual = crate::sha256_file(&zip)?;
        if actual != expected {
            return Err(UpdateError::new(format!("SHA-256 mismatch for {zip_name}")));
        }
        log(&format!("verified signed SHA-256 {actual}  {zip_name}"));
        let listing = Command::new("/usr/bin/zipinfo").arg("-1").arg(&zip).output()?;
        let entries: Vec<String> =
            String::from_utf8_lossy(&listing.stdout).lines().map(str::to_owned).collect();
        if !listing.status.success() || !archive_entries_are_safe(&entries, self.product.app_folder) {
            return Err(UpdateError::new(format!("unsafe or unexpected archive entries in {zip_name}")));
        }
        let unpacked = directory.join("unpacked");
        std::fs::create_dir(&unpacked)?;
        let status = Command::new("/usr/bin/ditto").args(["-x", "-k"]).arg(&zip).arg(&unpacked).status()?;
        if !status.success() {
            return Err(UpdateError::new(format!("could not unpack {zip_name}")));
        }
        let app = unpacked.join(self.product.app_folder);
        bundle::check_bundle(&app, &release.version, &self.product)?;
        bundle::reject_external_links(&app)?;
        self.identity.check(&app, &self.running_app)?;
        bundle::remove_quarantine(&app)?;
        log(&format!("verified signature: {} {}", self.product.app_folder, release.version));
        Ok(Staged { version: release.version.to_string(), directory: directory.to_path_buf(), app, sha256: actual })
    }
}

/// A new private (0700) `heimdall-update.XXXXXX` folder under `root`.
pub fn make_work_dir(root: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(root)?;
    let template = root.join("heimdall-update.XXXXXX");
    let c = CString::new(template.as_os_str().as_bytes()).map_err(|_| UpdateError::new("bad temp path"))?;
    let mut bytes = c.into_bytes_with_nul();
    // SAFETY: `bytes` is a writable NUL-terminated template that mkdtemp fills in place.
    let made = unsafe { libc::mkdtemp(bytes.as_mut_ptr().cast()) };
    if made.is_null() {
        return Err(UpdateError::new("could not create a temporary directory"));
    }
    bytes.pop();
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
}
